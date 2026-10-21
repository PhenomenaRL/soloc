//! Streaming GPX 1.1 input. This is an adapter, not a complete XSD validator.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::iter::FusedIterator;

use quick_xml::XmlVersion;
use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{QName, ResolveResult};
use quick_xml::reader::NsReader;

use super::{GpxContext, GpxError, GpxErrorKind, GpxEvent, GpxObjectId, GpxPoint, WaypointPolicy};

const GPX_NAMESPACE: &[u8] = b"http://www.topografix.com/GPX/1/1";
const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns/";
const MAX_DEPTH: usize = 128;
const MAX_FIELD_BYTES: usize = 1_048_576;

#[derive(Debug)]
enum Kind {
    Root,
    Metadata,
    Object,
    Segment,
    Point,
    Field { name: String, text: String },
    Ignored,
    Waypoint,
}

#[derive(Debug)]
struct Node {
    kind: Kind,
    fields: BTreeSet<String>,
}

impl Node {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            fields: BTreeSet::new(),
        }
    }
}

/// Read GPX 1.1 events in file order without retaining the whole document.
///
/// Both default and prefixed GPX namespaces are supported. GPX 1.0, absent or
/// incorrect namespaces, DTDs, non-UTF-8 declarations, malformed XML, duplicate
/// recognized scalar fields, and misplaced track/route/point elements fail with
/// source context. Latitude is in `[-90, 90]`, longitude in `[-180, 180)`, as in
/// the GPX schema. Elevation and time remain optional until mapping.
///
/// Unknown subtrees are reported once as [`GpxEvent::Ignored`]; their XML is still
/// checked, but their contents are not interpreted as points. This reader does
/// not validate complete XSD element ordering or ignored metadata against XSD.
/// Text fields are limited to 1 MiB and XML nesting to 128 elements. Names,
/// descriptions, comments, types, source labels, and raw quality fields survive
/// as metadata; quality fields do not imply covariance or measured attitude.
/// Unsupported attributes, comments, and processing instructions are checked for
/// XML syntax but are not retained or reported. Keep the input for full provenance.
///
/// `DocumentEnd` is emitted only after the root has closed and EOF was checked.
/// After the first error this iterator is exhausted. A consumer that stops early
/// has not validated the remainder of the document.
pub struct GpxReader<R: BufRead> {
    reader: NsReader<R>,
    buffer: Vec<u8>,
    source: String,
    waypoints: WaypointPolicy,
    stack: Vec<Node>,
    object: Option<GpxObjectId>,
    segment: Option<usize>,
    point_number: Option<usize>,
    point: Option<GpxPoint>,
    tracks: usize,
    routes: usize,
    segments: usize,
    points: usize,
    waypoints_seen: usize,
    root_seen: bool,
    root_closed: bool,
    declaration_seen: bool,
    prolog_content_seen: bool,
    done: bool,
}

impl<R: BufRead> GpxReader<R> {
    pub fn new(reader: R, source: impl Into<String>, waypoints: WaypointPolicy) -> Self {
        let mut reader = NsReader::from_reader(reader);
        reader.config_mut().expand_empty_elements = true;
        reader.config_mut().check_comments = true;
        Self {
            reader,
            buffer: Vec::new(),
            source: source.into(),
            waypoints,
            stack: Vec::new(),
            object: None,
            segment: None,
            point_number: None,
            point: None,
            tracks: 0,
            routes: 0,
            segments: 0,
            points: 0,
            waypoints_seen: 0,
            root_seen: false,
            root_closed: false,
            declaration_seen: false,
            prolog_content_seen: false,
            done: false,
        }
    }

    fn context(&self) -> GpxContext {
        GpxContext {
            source: self.source.clone(),
            object: self.object,
            segment: self.segment,
            point: self.point_number,
        }
    }

    fn error(&self, kind: GpxErrorKind) -> GpxError {
        GpxError {
            context: self.context(),
            kind,
        }
    }

    fn xml_error(&self, message: impl ToString) -> GpxError {
        self.error(GpxErrorKind::Xml(format!(
            "{} (byte {})",
            message.to_string(),
            self.reader.buffer_position()
        )))
    }

    fn structure_error(&self, message: impl Into<String>) -> GpxError {
        self.error(GpxErrorKind::Structure(message.into()))
    }

    fn parent(&self) -> Option<&Kind> {
        self.stack.last().map(|node| &node.kind)
    }

    fn push(&mut self, kind: Kind) -> Result<(), GpxError> {
        if self.stack.len() >= MAX_DEPTH {
            return Err(self.structure_error("XML nesting exceeds 128 elements"));
        }
        self.stack.push(Node::new(kind));
        Ok(())
    }

    fn record_field(&mut self, name: &str) -> Result<(), GpxError> {
        if let Some(parent) = self.stack.last_mut()
            && !parent.fields.insert(name.to_owned())
        {
            return Err(self.structure_error(format!("duplicate <{name}> field")));
        }
        Ok(())
    }

    fn attributes(&self, start: &BytesStart<'_>) -> Result<BTreeMap<String, String>, GpxError> {
        let mut result = BTreeMap::new();
        let mut expanded = BTreeSet::new();
        for attribute in start.attributes() {
            let attribute = attribute.map_err(|error| self.xml_error(error))?;
            let name = std::str::from_utf8(attribute.key.as_ref())
                .map_err(|error| self.xml_error(error))?;
            validate_qname(name).map_err(|error| self.xml_error(error))?;
            if attribute.value.contains(&b'<') {
                return Err(self.xml_error("unescaped '<' in an attribute"));
            }
            let value = attribute
                .decoded_and_normalized_value(XmlVersion::Implicit1_0, self.reader.decoder())
                .map_err(|error| self.xml_error(error))?;
            validate_chars(&value).map_err(|error| self.xml_error(error))?;
            if name == "xmlns" || name.starts_with("xmlns:") {
                let prefix = name.strip_prefix("xmlns:");
                if prefix == Some("xmlns")
                    || value == XMLNS_NAMESPACE
                    || (value == XML_NAMESPACE && prefix != Some("xml"))
                    || (prefix == Some("xml") && value != XML_NAMESPACE)
                    || (prefix.is_some() && value.is_empty())
                {
                    return Err(self.xml_error("invalid reserved or empty namespace binding"));
                }
                continue;
            }
            let (namespace, local_name) = self.reader.resolver().resolve_attribute(attribute.key);
            let namespace = match namespace {
                ResolveResult::Bound(namespace) => normalize_namespace(namespace.as_ref())
                    .map_err(|error| self.xml_error(error))?,
                ResolveResult::Unbound => Vec::new(),
                ResolveResult::Unknown(prefix) => {
                    return Err(self.xml_error(format!(
                        "undeclared attribute namespace prefix '{}'",
                        String::from_utf8_lossy(&prefix)
                    )));
                }
            };
            if !expanded.insert((namespace, local_name.as_ref().to_vec())) {
                return Err(self.xml_error(format!("duplicate expanded attribute '{name}'")));
            }
            result.insert(name.to_owned(), value.into_owned());
        }
        Ok(result)
    }

    fn begin(
        &mut self,
        start: BytesStart<'_>,
        namespace: Option<Vec<u8>>,
    ) -> Result<Option<GpxEvent>, GpxError> {
        let local_name = start.local_name();
        validate_qname(
            std::str::from_utf8(start.name().as_ref()).map_err(|error| self.xml_error(error))?,
        )
        .map_err(|error| self.xml_error(error))?;
        if start.name().as_ref().starts_with(b"xmlns:") {
            return Err(self.xml_error("the xmlns prefix cannot be used on elements"));
        }
        let name = std::str::from_utf8(local_name.as_ref())
            .map_err(|error| self.xml_error(error))?
            .to_owned();
        let in_gpx = namespace.as_deref() == Some(GPX_NAMESPACE);

        // Establish point context before checking its attributes, including missing ones.
        if in_gpx
            && ((name == "trkpt" && matches!(self.parent(), Some(Kind::Segment)))
                || (name == "rtept"
                    && matches!(self.object, Some(GpxObjectId::Route(_)))
                    && matches!(self.parent(), Some(Kind::Object))))
        {
            self.points += 1;
            self.point_number = Some(self.points);
        }
        let attributes = self.attributes(&start)?;
        if self.root_closed {
            return Err(self.structure_error("multiple root elements are not allowed"));
        }
        if matches!(self.parent(), Some(Kind::Ignored | Kind::Waypoint)) {
            self.push(Kind::Ignored)?;
            return Ok(None);
        }
        if matches!(self.parent(), Some(Kind::Field { .. })) {
            return Err(self.structure_error("scalar GPX fields cannot contain child elements"));
        }
        if !self.root_seen {
            if name != "gpx" || !in_gpx {
                return Err(self.error(GpxErrorKind::Unsupported(
                    "expected <gpx> in namespace http://www.topografix.com/GPX/1/1".into(),
                )));
            }
            let version = attributes
                .get("version")
                .ok_or_else(|| self.error(GpxErrorKind::MissingField("version")))?;
            if version != "1.1" {
                return Err(self.error(GpxErrorKind::Unsupported(format!(
                    "GPX version '{version}'; only GPX 1.1 is supported"
                ))));
            }
            let creator = attributes
                .get("creator")
                .ok_or_else(|| self.error(GpxErrorKind::MissingField("creator")))?
                .clone();
            self.root_seen = true;
            self.push(Kind::Root)?;
            return Ok(Some(GpxEvent::DocumentStart { creator }));
        }
        if !in_gpx {
            return self.ignore(namespace, &name);
        }

        match name.as_str() {
            "trk" | "rte" if matches!(self.parent(), Some(Kind::Root)) => {
                self.object = Some(if name == "trk" {
                    self.tracks += 1;
                    GpxObjectId::Track(self.tracks)
                } else {
                    self.routes += 1;
                    GpxObjectId::Route(self.routes)
                });
                self.segments = 0;
                self.points = 0;
                self.push(Kind::Object)?;
                Ok(Some(GpxEvent::ObjectStart {
                    context: self.context(),
                }))
            }
            "trkseg"
                if matches!(self.parent(), Some(Kind::Object))
                    && matches!(self.object, Some(GpxObjectId::Track(_))) =>
            {
                self.segments += 1;
                self.segment = Some(self.segments);
                self.points = 0;
                self.push(Kind::Segment)?;
                Ok(Some(GpxEvent::SegmentStart {
                    context: self.context(),
                }))
            }
            "trkpt" if matches!(self.parent(), Some(Kind::Segment)) => self.begin_point(attributes),
            "rtept"
                if matches!(self.parent(), Some(Kind::Object))
                    && matches!(self.object, Some(GpxObjectId::Route(_))) =>
            {
                self.begin_point(attributes)
            }
            "wpt" if matches!(self.parent(), Some(Kind::Root)) => {
                self.waypoints_seen += 1;
                let mut context = self.context();
                context.point = Some(self.waypoints_seen);
                match self.waypoints {
                    WaypointPolicy::Reject => Err(GpxError {
                        context,
                        kind: GpxErrorKind::Unsupported(
                            "standalone <wpt> requires explicit WaypointPolicy::Ignore".into(),
                        ),
                    }),
                    WaypointPolicy::Ignore => {
                        self.point_number = Some(self.waypoints_seen);
                        self.push(Kind::Waypoint)?;
                        Ok(Some(GpxEvent::Ignored {
                            context,
                            element: "wpt".into(),
                        }))
                    }
                }
            }
            "gpx" | "trk" | "rte" | "trkseg" | "trkpt" | "rtept" | "wpt" => {
                Err(self.structure_error(format!("misplaced <{name}> element")))
            }
            "metadata" if matches!(self.parent(), Some(Kind::Root)) => {
                self.record_field(&name)?;
                self.push(Kind::Metadata)?;
                Ok(None)
            }
            _ if recognized_field(self.parent(), &name) => {
                self.record_field(&name)?;
                self.push(Kind::Field {
                    name,
                    text: String::new(),
                })?;
                Ok(None)
            }
            _ => {
                if name == "extensions" {
                    self.record_field(&name)?;
                }
                self.ignore(namespace, &name)
            }
        }
    }

    fn ignore(
        &mut self,
        namespace: Option<Vec<u8>>,
        name: &str,
    ) -> Result<Option<GpxEvent>, GpxError> {
        let element = match namespace {
            Some(namespace) if namespace != GPX_NAMESPACE => {
                format!("{{{}}}{name}", String::from_utf8_lossy(&namespace))
            }
            None => format!("{{}}{name}"),
            _ => name.to_owned(),
        };
        self.push(Kind::Ignored)?;
        Ok(Some(GpxEvent::Ignored {
            context: self.context(),
            element,
        }))
    }

    fn begin_point(
        &mut self,
        attributes: BTreeMap<String, String>,
    ) -> Result<Option<GpxEvent>, GpxError> {
        let coordinate = |field, lower, upper, upper_inclusive: bool| {
            let value = attributes
                .get(field)
                .ok_or_else(|| self.error(GpxErrorKind::MissingField(field)))?;
            let number = self.number(field, value)?;
            if number < lower || number > upper || (!upper_inclusive && number == upper) {
                return Err(self.error(GpxErrorKind::InvalidField {
                    field,
                    message: format!("out of range: {number}"),
                }));
            }
            Ok(number)
        };
        let latitude_deg = coordinate("lat", -90.0, 90.0, true)?;
        let longitude_deg = coordinate("lon", -180.0, 180.0, false)?;
        self.point = Some(GpxPoint {
            context: self.context(),
            latitude_deg,
            longitude_deg,
            elevation_m: None,
            geoid_height_m: None,
            time: None,
            metadata: BTreeMap::new(),
        });
        self.push(Kind::Point)?;
        Ok(None)
    }

    fn number(&self, field: &'static str, text: &str) -> Result<f64, GpxError> {
        decimal(text).map_err(|message| self.error(GpxErrorKind::InvalidField { field, message }))
    }

    fn end(&mut self) -> Result<Option<GpxEvent>, GpxError> {
        let node = self
            .stack
            .pop()
            .ok_or_else(|| self.structure_error("unexpected closing element"))?;
        match node.kind {
            Kind::Root => {
                self.root_closed = true;
                Ok(None)
            }
            Kind::Metadata | Kind::Ignored => Ok(None),
            Kind::Waypoint => {
                self.point_number = None;
                Ok(None)
            }
            Kind::Object => {
                let context = self.context();
                self.object = None;
                Ok(Some(GpxEvent::ObjectEnd { context }))
            }
            Kind::Segment => {
                let context = self.context();
                self.segment = None;
                Ok(Some(GpxEvent::SegmentEnd { context }))
            }
            Kind::Point => {
                let point = self.point.take().expect("point state follows XML stack");
                self.point_number = None;
                Ok(Some(GpxEvent::Point(point)))
            }
            Kind::Field { name, text } => self.finish_field(name, text),
        }
    }

    fn finish_field(&mut self, name: String, text: String) -> Result<Option<GpxEvent>, GpxError> {
        if matches!(self.parent(), Some(Kind::Point)) {
            match name.as_str() {
                "ele" => {
                    let value = self.number("ele", &text)?;
                    self.point.as_mut().expect("point state").elevation_m = Some(value);
                }
                "geoidheight" => {
                    let value = self.number("geoidheight", &text)?;
                    self.point.as_mut().expect("point state").geoid_height_m = Some(value);
                }
                "time" => {
                    self.point.as_mut().expect("point state").time =
                        Some(text.trim_matches([' ', '\t', '\r', '\n']).into());
                }
                _ => {
                    self.point
                        .as_mut()
                        .expect("point state")
                        .metadata
                        .insert(name, text);
                }
            }
            Ok(None)
        } else {
            Ok(Some(GpxEvent::Metadata {
                context: self.context(),
                field: name,
                value: text,
            }))
        }
    }

    fn text(&mut self, text: &str) -> Result<(), GpxError> {
        validate_chars(text).map_err(|error| self.xml_error(error))?;
        match self.stack.last_mut().map(|node| &mut node.kind) {
            Some(Kind::Field { text: value, .. }) => {
                if value.len().saturating_add(text.len()) > MAX_FIELD_BYTES {
                    return Err(self.structure_error("scalar field exceeds 1 MiB"));
                }
                value.push_str(text);
            }
            Some(Kind::Ignored | Kind::Waypoint) => {}
            _ if !text.trim_matches([' ', '\t', '\r', '\n']).is_empty() => {
                return Err(self.structure_error("unexpected text outside a scalar GPX field"));
            }
            _ => {}
        }
        Ok(())
    }

    fn read_next(&mut self) -> Result<Option<GpxEvent>, GpxError> {
        loop {
            self.buffer.clear();
            let (namespace, event) = self
                .reader
                .read_resolved_event_into(&mut self.buffer)
                .map_err(|error| GpxError {
                    context: GpxContext {
                        source: self.source.clone(),
                        object: self.object,
                        segment: self.segment,
                        point: self.point_number,
                    },
                    kind: GpxErrorKind::Xml(error.to_string()),
                })?;
            let event = event.into_owned();
            let namespace = match namespace {
                ResolveResult::Bound(namespace) => {
                    let normalized = normalize_namespace(namespace.as_ref());
                    Some(normalized.map_err(|error| self.xml_error(error))?)
                }
                ResolveResult::Unbound => None,
                ResolveResult::Unknown(prefix) => {
                    let prefix = String::from_utf8_lossy(&prefix).into_owned();
                    return Err(self.xml_error(format!("undeclared namespace prefix '{prefix}'")));
                }
            };
            match event {
                Event::Start(start) => {
                    if let Some(event) = self.begin(start, namespace)? {
                        return Ok(Some(event));
                    }
                }
                Event::End(_) => {
                    if let Some(event) = self.end()? {
                        return Ok(Some(event));
                    }
                }
                Event::Text(text) => {
                    if text.as_ref().windows(3).any(|window| window == b"]]>") {
                        return Err(self.xml_error("unescaped ']]>' in character data"));
                    }
                    let text = text
                        .xml10_content()
                        .map_err(|error| self.xml_error(error))?;
                    if !self.root_seen && !text.is_empty() {
                        self.prolog_content_seen = true;
                    }
                    self.text(&text)?;
                }
                Event::CData(text) => {
                    if self.stack.is_empty() {
                        return Err(self.xml_error("CDATA is not allowed outside the root"));
                    }
                    let text = text
                        .xml10_content()
                        .map_err(|error| self.xml_error(error))?;
                    self.text(&text)?;
                }
                Event::GeneralRef(reference) => {
                    if self.stack.is_empty() {
                        return Err(
                            self.xml_error("entity references are not allowed outside the root")
                        );
                    }
                    let value = match reference
                        .resolve_char_ref()
                        .map_err(|error| self.xml_error(error))?
                    {
                        Some(value) => value,
                        None => match reference.as_ref() {
                            b"amp" => '&',
                            b"lt" => '<',
                            b"gt" => '>',
                            b"apos" => '\'',
                            b"quot" => '"',
                            _ => return Err(self.xml_error("unknown XML entity reference")),
                        },
                    };
                    self.text(&value.to_string())?;
                }
                Event::Decl(declaration) => {
                    if self.root_seen || self.declaration_seen || self.prolog_content_seen {
                        return Err(self.xml_error("XML declaration must appear once at the start"));
                    }
                    let version = declaration
                        .version()
                        .map_err(|error| self.xml_error(error))?;
                    if version.as_ref() != b"1.0" {
                        return Err(self.error(GpxErrorKind::Unsupported(
                            "only XML 1.0 is supported".into(),
                        )));
                    }
                    if let Some(encoding) = declaration.encoding() {
                        let encoding = encoding.map_err(|error| self.xml_error(error))?;
                        if !encoding.eq_ignore_ascii_case(b"utf-8") {
                            return Err(self.error(GpxErrorKind::Unsupported(
                                "only UTF-8 XML is supported".into(),
                            )));
                        }
                    }
                    let raw = std::str::from_utf8(declaration.as_ref())
                        .map_err(|error| self.xml_error(error))?;
                    let start = BytesStart::from_content(raw, 3);
                    let mut previous_order = 0;
                    for attribute in start.attributes() {
                        let attribute = attribute.map_err(|error| self.xml_error(error))?;
                        let order = match attribute.key.as_ref() {
                            b"version" => 1,
                            b"encoding" => 2,
                            b"standalone" => 3,
                            _ => return Err(self.xml_error("unexpected XML declaration attribute")),
                        };
                        if order <= previous_order || attribute.value.contains(&b'&') {
                            return Err(
                                self.xml_error("invalid XML declaration attribute order or value")
                            );
                        }
                        previous_order = order;
                    }
                    let attributes = self.attributes(&start)?;
                    if attributes
                        .keys()
                        .any(|name| !matches!(name.as_str(), "version" | "encoding" | "standalone"))
                    {
                        return Err(self.xml_error("unexpected XML declaration attribute"));
                    }
                    if let Some(standalone) = attributes.get("standalone")
                        && !matches!(standalone.as_str(), "yes" | "no")
                    {
                        return Err(self.xml_error("standalone must be 'yes' or 'no'"));
                    }
                    self.declaration_seen = true;
                }
                Event::DocType(_) => {
                    return Err(self.error(GpxErrorKind::Unsupported(
                        "XML DTDs are not supported".into(),
                    )));
                }
                Event::Comment(comment) => {
                    let text = comment
                        .xml10_content()
                        .map_err(|error| self.xml_error(error))?;
                    validate_chars(&text).map_err(|error| self.xml_error(error))?;
                    if !self.root_seen {
                        self.prolog_content_seen = true;
                    }
                }
                Event::PI(instruction) => {
                    if instruction.target().eq_ignore_ascii_case(b"xml") {
                        return Err(
                            self.xml_error("processing instruction target 'xml' is reserved")
                        );
                    }
                    let target = std::str::from_utf8(instruction.target())
                        .map_err(|error| self.xml_error(error))?;
                    if !valid_name(target, true) {
                        return Err(self.xml_error("invalid XML processing instruction name"));
                    }
                    let text = std::str::from_utf8(instruction.as_ref())
                        .map_err(|error| self.xml_error(error))?;
                    validate_chars(text).map_err(|error| self.xml_error(error))?;
                    if !self.root_seen {
                        self.prolog_content_seen = true;
                    }
                }
                Event::Eof => {
                    if !self.root_seen {
                        return Err(self.structure_error("empty document: expected a GPX root"));
                    }
                    if !self.root_closed || !self.stack.is_empty() {
                        return Err(self.xml_error("unexpected EOF before the GPX root closed"));
                    }
                    self.done = true;
                    return Ok(Some(GpxEvent::DocumentEnd));
                }
                Event::Empty(_) => unreachable!("empty elements are expanded by the reader"),
            }
        }
    }
}

impl<R: BufRead> Iterator for GpxReader<R> {
    type Item = Result<GpxEvent, GpxError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.read_next() {
            Ok(event) => event.map(Ok),
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }
}

impl<R: BufRead> FusedIterator for GpxReader<R> {}

fn recognized_field(parent: Option<&Kind>, name: &str) -> bool {
    match parent {
        Some(Kind::Metadata) => matches!(name, "name" | "desc" | "time" | "keywords"),
        Some(Kind::Object) => matches!(name, "name" | "cmt" | "desc" | "src" | "number" | "type"),
        Some(Kind::Point) => matches!(
            name,
            "ele"
                | "time"
                | "magvar"
                | "geoidheight"
                | "name"
                | "cmt"
                | "desc"
                | "src"
                | "sym"
                | "type"
                | "fix"
                | "sat"
                | "hdop"
                | "vdop"
                | "pdop"
                | "ageofdgpsdata"
                | "dgpsid"
        ),
        _ => false,
    }
}

fn validate_chars(text: &str) -> Result<(), &'static str> {
    if text.chars().all(|ch| {
        matches!(ch, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
    }) {
        Ok(())
    } else {
        Err("character is not allowed in XML 1.0")
    }
}

fn normalize_namespace(raw: &[u8]) -> Result<Vec<u8>, String> {
    // NsReader returns raw xmlns values, so namespace identity must use XML's
    // attribute normalization too (e.g. "urn:x" equals "urn:&#120;").
    let attribute = Attribute {
        key: QName(b"xmlns"),
        value: Cow::Borrowed(raw),
    };
    let value = attribute
        .normalized_value(XmlVersion::Implicit1_0)
        .map_err(|error| error.to_string())?;
    validate_chars(&value).map_err(str::to_owned)?;
    Ok(value.as_bytes().to_vec())
}

fn validate_qname(name: &str) -> Result<(), &'static str> {
    let mut parts = name.split(':');
    if !parts.next().is_some_and(|part| valid_name(part, false))
        || parts.next().is_some_and(|part| !valid_name(part, false))
        || parts.next().is_some()
    {
        Err("invalid XML qualified name")
    } else {
        Ok(())
    }
}

// XML 1.0 Fifth Edition NameStartChar/NameChar. QNames validate each NCName
// separately; processing-instruction names allow colons without namespace meaning.
fn valid_name(name: &str, allow_colon: bool) -> bool {
    let start = |ch| {
        (allow_colon && ch == ':')
            || matches!(ch, 'A'..='Z' | '_' | 'a'..='z' | '\u{c0}'..='\u{d6}'
                | '\u{d8}'..='\u{f6}' | '\u{f8}'..='\u{2ff}' | '\u{370}'..='\u{37d}'
                | '\u{37f}'..='\u{1fff}' | '\u{200c}'..='\u{200d}' | '\u{2070}'..='\u{218f}'
                | '\u{2c00}'..='\u{2fef}' | '\u{3001}'..='\u{d7ff}' | '\u{f900}'..='\u{fdcf}'
                | '\u{fdf0}'..='\u{fffd}' | '\u{10000}'..='\u{effff}')
    };
    let mut chars = name.chars();
    chars.next().is_some_and(start)
        && chars.all(|ch| start(ch) || matches!(ch, '-' | '.' | '0'..='9' | '\u{b7}' | '\u{300}'..='\u{36f}' | '\u{203f}'..='\u{2040}'))
}

fn decimal(text: &str) -> Result<f64, String> {
    let text = text.trim_matches([' ', '\t', '\r', '\n']);
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let mut digits = 0;
    let mut dots = 0;
    for byte in unsigned.bytes() {
        if byte.is_ascii_digit() {
            digits += 1;
        } else if byte == b'.' {
            dots += 1;
        } else {
            return Err(format!("expected an XML decimal, got '{text}'"));
        }
    }
    if digits == 0 || dots > 1 {
        return Err(format!("expected an XML decimal, got '{text}'"));
    }
    let number: f64 = text
        .parse()
        .map_err(|error| format!("invalid decimal: {error}"))?;
    if !number.is_finite() {
        return Err("decimal is outside finite f64 range".into());
    }
    Ok(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: &str =
        r#"<gpx xmlns="http://www.topografix.com/GPX/1/1" version="1.1" creator="fixture">"#;

    fn document(body: &str) -> String {
        format!("{OPEN}{body}</gpx>")
    }

    fn read(xml: &str) -> Result<Vec<GpxEvent>, GpxError> {
        GpxReader::new(xml.as_bytes(), "fixture.gpx", WaypointPolicy::Reject).collect()
    }

    fn point_xml(fields: &str) -> String {
        document(&format!(
            r#"<trk><trkseg><trkpt lat="44.6" lon="-63.5">{fields}</trkpt></trkseg></trk>"#
        ))
    }

    #[test]
    fn preserves_file_order_repeated_points_and_empty_segment_boundaries() {
        let xml = document(
            r#"<rte><name>route first</name><rtept lat="1" lon="2"/></rte>
            <trk><trkseg/><trkseg>
            <trkpt lat="3" lon="4"><time>2025-01-02T00:00:00Z</time></trkpt>
            <trkpt lat="3" lon="4"><time>2025-01-02T00:00:00Z</time></trkpt>
            <trkpt lat="5" lon="6"><time>2025-01-01T00:00:00Z</time></trkpt>
            </trkseg></trk><rte/><trk/>"#,
        );
        let events = read(&xml).unwrap();
        let objects: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                GpxEvent::ObjectStart { context } => context.object,
                _ => None,
            })
            .collect();
        assert_eq!(
            objects,
            [
                GpxObjectId::Route(1),
                GpxObjectId::Track(1),
                GpxObjectId::Route(2),
                GpxObjectId::Track(2)
            ]
        );
        let segments: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                GpxEvent::SegmentStart { context } => Some(("start", context.segment)),
                GpxEvent::SegmentEnd { context } => Some(("end", context.segment)),
                _ => None,
            })
            .collect();
        assert_eq!(
            segments,
            [
                ("start", Some(1)),
                ("end", Some(1)),
                ("start", Some(2)),
                ("end", Some(2))
            ]
        );
        let points: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                GpxEvent::Point(point) => Some(point),
                _ => None,
            })
            .collect();
        assert_eq!(points.len(), 4);
        assert_eq!(points[0].context.segment, None);
        assert_eq!(points[0].context.point, Some(1));
        assert_eq!(points[1].context.segment, Some(2));
        assert_eq!(points[1].context.point, Some(1));
        assert_eq!(points[2].context.point, Some(2));
        assert_eq!(points[3].context.point, Some(3));
        assert_eq!(points[1].time, points[2].time);
        assert!(points[3].time < points[2].time);
        assert_eq!(events.last(), Some(&GpxEvent::DocumentEnd));
    }

    #[test]
    fn namespaces_are_resolved_and_foreign_lookalikes_are_never_imported() {
        let xml = r#"<g:gpx xmlns:g="http://www.topografix.com/GPX/1/1" xmlns:x="urn:vendor" version="1.1" creator="fixture">
          <x:trk><g:trk><g:trkseg><g:trkpt lat="1" lon="2"/></g:trkseg></g:trk></x:trk>
          <g:trk><g:trkseg><g:trkpt lat="3" lon="4">
            <x:time>wrong</x:time><time xmlns="urn:other">wrong</time>
            <g:extensions><g:time>wrong</g:time></g:extensions>
            <g:time>2025-01-01T00:00:00Z</g:time>
          </g:trkpt></g:trkseg></g:trk>
        </g:gpx>"#;
        let events = read(xml).unwrap();
        let points: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                GpxEvent::Point(point) => Some(point),
                _ => None,
            })
            .collect();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].context.object, Some(GpxObjectId::Track(1)));
        assert_eq!(points[0].latitude_deg, 3.0);
        assert_eq!(points[0].time.as_deref(), Some("2025-01-01T00:00:00Z"));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, GpxEvent::Ignored { .. }))
                .count(),
            4
        );
        let escaped = r#"<gpx xmlns="http://www.topografix.com/GPX/1/&#49;" version="1.1" creator="fixture"><rte><rtept lat="1" lon="2"/></rte></gpx>"#;
        assert_eq!(
            read(escaped)
                .unwrap()
                .iter()
                .filter(|event| matches!(event, GpxEvent::Point(_)))
                .count(),
            1
        );
    }

    #[test]
    fn scalar_chunks_entities_and_raw_quality_metadata_are_preserved() {
        let xml = document(
            r#"<metadata><name>A &amp; B</name><author><name>Ignored</name></author></metadata>
            <trk><name>Journey &#x31;</name><desc> left <![CDATA[<&>]]> right </desc>
            <trkseg><trkpt lat="&#52;4.6" lon="-63.5">
            <ele> -12.50 </ele><geoidheight>+2.5</geoidheight>
            <time> 2025-01-01T00:00:00Z </time><name>A &amp; B</name>
            <hdop> not a covariance </hdop><fix>3d</fix><sat>7</sat>
            </trkpt></trkseg></trk>"#,
        );
        let events = read(&xml).unwrap();
        assert!(events.iter().any(|event| matches!(event,
            GpxEvent::Metadata { field, value, .. } if field == "name" && value == "A & B")));
        assert!(events.iter().any(|event| matches!(event,
            GpxEvent::Metadata { field, value, .. } if field == "desc" && value == " left <&> right ")));
        let point = events
            .into_iter()
            .find_map(|event| match event {
                GpxEvent::Point(point) => Some(point),
                _ => None,
            })
            .unwrap();
        assert_eq!(point.latitude_deg, 44.6);
        assert_eq!(point.elevation_m, Some(-12.5));
        assert_eq!(point.geoid_height_m, Some(2.5));
        assert_eq!(point.time.as_deref(), Some("2025-01-01T00:00:00Z"));
        assert_eq!(point.metadata["name"], "A & B");
        assert_eq!(point.metadata["hdop"], " not a covariance ");
        assert_eq!(point.metadata["fix"], "3d");
        assert_eq!(point.metadata["sat"], "7");
    }

    #[test]
    fn rejects_unsupported_roots_versions_and_missing_required_attributes() {
        for xml in [
            "",
            "   ",
            "<not-gpx/>",
            r#"<gpx version="1.1" creator="fixture"/>"#,
            r#"<gpx xmlns="http://www.topografix.com/GPX/1/0" version="1.0" creator="fixture"/>"#,
            r#"<gpx xmlns="http://www.topografix.com/GPX/1/1" version="1.0" creator="fixture"/>"#,
            r#"<gpx xmlns="http://www.topografix.com/GPX/1/1" version="1.1"/>"#,
            r#"<gpx xmlns="http://www.topografix.com/GPX/1/1" creator="fixture"/>"#,
            r#"<gpx xmlns="http://www.topografix.com/GPX/1/1" xmlns:x="urn:attributes" x:version="1.1" creator="fixture"/>"#,
        ] {
            let error = read(xml).expect_err(xml);
            assert_eq!(error.context.source, "fixture.gpx");
        }
        let events = read(&document("")).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events.last(), Some(&GpxEvent::DocumentEnd));
    }

    #[test]
    fn rejects_misplaced_structure_and_duplicate_fields() {
        for body in [
            r#"<trkpt lat="1" lon="2"/>"#,
            r#"<trk><trkpt lat="1" lon="2"/></trk>"#,
            r#"<rte><trkseg/></rte>"#,
            r#"<trk><trkseg><rtept lat="1" lon="2"/></trkseg></trk>"#,
            r#"<rte><rte/></rte>"#,
            r#"<metadata/><metadata/>"#,
            r#"<trk><name>A</name><name>B</name></trk>"#,
            r#"<trk><name>A<desc>B</desc></name></trk>"#,
        ] {
            assert!(
                matches!(
                    read(&document(body)).unwrap_err().kind,
                    GpxErrorKind::Structure(_)
                ),
                "{body}"
            );
        }
        for field in ["ele", "time", "geoidheight", "name", "hdop", "extensions"] {
            let fields = format!("<{field}>1</{field}><{field}>2</{field}>");
            let error = read(&point_xml(&fields)).unwrap_err();
            assert!(matches!(error.kind, GpxErrorKind::Structure(_)), "{error}");
            assert_eq!(error.context.object, Some(GpxObjectId::Track(1)));
            assert_eq!(error.context.segment, Some(1));
            assert_eq!(error.context.point, Some(1));
        }
    }

    #[test]
    fn missing_and_invalid_coordinates_have_the_original_point_context() {
        for attributes in [
            r#"lon="2""#,
            r#"lat="1""#,
            r#"lat="91" lon="2""#,
            r#"lat="1" lon="180""#,
            r#"lat="1" lon="-181""#,
            r#"lat="NaN" lon="2""#,
            r#"lat="1e1" lon="2""#,
            r#"lat="1" lat="2" lon="3""#,
        ] {
            let xml = document(&format!(
                r#"<trk><trkseg/><trkseg><trkpt lat="1" lon="2"/><trkpt {attributes}/></trkseg></trk>"#
            ));
            let error = read(&xml).unwrap_err();
            assert_eq!(error.context.object, Some(GpxObjectId::Track(1)));
            assert_eq!(error.context.segment, Some(2));
            assert_eq!(error.context.point, Some(2), "{error}");
        }
        assert!(
            read(&document(
                r#"<rte><rtept lat="-90" lon="-180"/><rtept lat="90" lon="179.999"/></rte>"#
            ))
            .is_ok()
        );
        for value in ["", "NaN", "inf", "1e3", "1.2.3", "+", "--1"] {
            assert!(
                read(&point_xml(&format!("<ele>{value}</ele>"))).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn standalone_waypoint_policy_is_explicit_and_ignore_still_checks_xml() {
        let xml = document(
            r#"<wpt lat="1" lon="2"><name>Place</name></wpt><rte><rtept lat="3" lon="4"/></rte>"#,
        );
        let error = read(&xml).unwrap_err();
        assert!(matches!(error.kind, GpxErrorKind::Unsupported(_)));
        assert_eq!(error.context.point, Some(1));
        let events: Vec<_> = GpxReader::new(xml.as_bytes(), "waypoint.gpx", WaypointPolicy::Ignore)
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            events.iter().any(
                |event| matches!(event, GpxEvent::Ignored { element, .. } if element == "wpt")
            )
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, GpxEvent::Point(_)))
                .count(),
            1
        );
        let invalid = document("<wpt><name>&unknown;</name></wpt>");
        let error = GpxReader::new(invalid.as_bytes(), "waypoint.gpx", WaypointPolicy::Ignore)
            .collect::<Result<Vec<_>, _>>()
            .unwrap_err();
        assert!(matches!(error.kind, GpxErrorKind::Xml(_)));
        assert_eq!(error.context.point, Some(1));
    }

    #[test]
    fn malformed_xml_fails_even_inside_ignored_subtrees_and_iterator_is_fused() {
        for xml in [
            format!("{OPEN}<trk>"),
            document("<trk></rte>"),
            format!("{}trailing", document("")),
            format!("{}{}", document(""), document("")),
            document("<extensions><x:unknown/></extensions>"),
            document("<extensions><unknown>&missing;</unknown></extensions>"),
            document("<extensions><unknown>&#1;</unknown></extensions>"),
            document("<extensions><unknown>\u{1}</unknown></extensions>"),
            document("<extensions><unknown>]]></unknown></extensions>"),
            document("<extensions><1invalid/></extensions>"),
            document(
                r#"<extensions xmlns:a="urn:x" xmlns:b="urn:x"><unknown a:x="1" b:x="2"/></extensions>"#,
            ),
            document(
                r#"<extensions xmlns:a="urn:x" xmlns:b="urn:&#120;"><unknown a:x="1" b:x="2"/></extensions>"#,
            ),
            document(r#"<extensions xmlns:a=""/>"#),
            document(r#"<extensions xmlns:a="http://www.w3.org/XML/1998/namespac&#101;"/>"#),
            document(r#"<extensions xmlns="http://www.w3.org/2000/xmlns&#47;"/>"#),
            document(r#"<xmlns:forbidden/>"#),
            document(r#"<extensions><unknown x="&missing;"/></extensions>"#),
            document(r#"<extensions><unknown x="<"/></extensions>"#),
            document("<!--bad--comment-->"),
            format!("<!DOCTYPE gpx [<!ENTITY x 'entity'>]>{}", document("")),
            format!(r#"<?xml version="1.0" encoding="UTF-16"?>{}"#, document("")),
            format!(r#"<?xml version="1.1"?>{}"#, document("")),
            format!(r#"<?xml version="1.0" version="1.1"?>{}"#, document("")),
            format!(r#"<?xml version="1.0" xmlns:x="urn:x"?>{}"#, document("")),
            format!(
                r#"<?xml version="1.0" standalone="yes" encoding="UTF-8"?>{}"#,
                document("")
            ),
            format!(r#"<?XML version="1.0"?>{}"#, document("")),
            document(r#"<?xml version="1.0"?>"#),
        ] {
            let mut reader = GpxReader::new(xml.as_bytes(), "bad.gpx", WaypointPolicy::Reject);
            let mut failed = false;
            for item in reader.by_ref() {
                match item {
                    Err(_) => {
                        failed = true;
                        break;
                    }
                    Ok(GpxEvent::DocumentEnd) => panic!("malformed input completed: {xml}"),
                    _ => {}
                }
            }
            assert!(failed, "accepted malformed input: {xml}");
            assert!(reader.next().is_none());
            assert!(reader.next().is_none());
        }
    }

    #[test]
    fn utf8_bom_declaration_comments_and_processing_instructions_are_supported() {
        let xml = format!(
            "\u{feff}<?xml version='1.0' encoding='UTF-8' standalone='yes'?>\n<!-- prolog --><?fixture example?>{}<!--end-->",
            document("<rte><name>Montréal</name></rte>")
        );
        let events = read(&xml).unwrap();
        assert!(
            events.iter().any(
                |event| matches!(event, GpxEvent::Metadata { value, .. } if value == "Montréal")
            )
        );
        let mut invalid_utf8 = document("<extensions>bad</extensions>").into_bytes();
        let index = invalid_utf8
            .windows(3)
            .position(|bytes| bytes == b"bad")
            .unwrap();
        invalid_utf8[index] = 0xff;
        assert!(
            GpxReader::new(invalid_utf8.as_slice(), "bad.gpx", WaypointPolicy::Ignore)
                .collect::<Result<Vec<_>, _>>()
                .is_err()
        );
    }

    #[test]
    fn parser_bounds_nesting_and_scalar_storage() {
        let nested = document(&format!(
            "<extensions>{}{} </extensions>",
            "<x>".repeat(MAX_DEPTH),
            "</x>".repeat(MAX_DEPTH)
        ));
        assert!(matches!(
            read(&nested).unwrap_err().kind,
            GpxErrorKind::Structure(_)
        ));
        let large = document(&format!(
            "<trk><name>{}</name></trk>",
            "x".repeat(MAX_FIELD_BYTES + 1)
        ));
        assert!(matches!(
            read(&large).unwrap_err().kind,
            GpxErrorKind::Structure(_)
        ));
    }
}
