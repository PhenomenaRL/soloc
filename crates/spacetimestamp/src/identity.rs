//! Prescribed identity: deterministic 16-byte ids for frames, entities, and sources.
//!
//! Identity here is a **pure function**.An id is derived from `(authority, common_name, kind)`
//! by hashing.
//!
//! # Layout
//!
//! ```text
//! byte   0  1  2  3  4  5   6   7   8   9 10 11 12 13 14 15
//!       [---- hash 48 ----][V|K][h8][R|-------- hash 62 --------]
//!
//! byte 6 high nibble = 0x8    version 8      (fixed, RFC 9562)
//! byte 6 low  nibble = kind   discriminator
//! byte 8 high 2 bits = 0b10   variant        (fixed, RFC 9562)
//! remaining 118 bits = truncated SHA-256
//! ```
//!
//! UUIDv8 is used to keep minted PrescribedIds *valid UUID*: `uuid.UUID` in Python, standard
//! 8-4-4-4-12 formatting./!
//!
//! `kind` is inside the hash as well as the nibble: flipping the
//! nibble yields an id whose body no longer verifies, and a reserved authority can never
//! collide with an operator who happens to pick the same string; the reserved namespace
//! needs no policing.
//!
//! # Arrow representation
//!
//! Id columns are plain `FixedSizeBinary(16)` carrying `ARROW:extension:name = arrow.uuid`.
//!
//! The small closed vocabularies inside a spacetimestamp (`units_pos`, `timescale_id`,
//! `estimate_type`) are `UInt8` codes carrying their decode table in field metadata; see
//! [`crate::vocabulary`].
//!
//! # Cross-language contract
//!
//! Other implementations (the `pyarrow` client, the visualizer etc...) must mint byte-identical
//! ids. Rules are as follows:
//!
//! 1. Lowercase the authority, **ASCII-only** — Unicode case folding varies between
//!    languages and versions, and an id must not depend on which one you have.
//! 2. Hash `NAMESPACE ‖ [kind] ‖ authority ‖ 0x00 ‖ name` with SHA-256.
//! 3. Take the first 16 bytes and stamp the version, kind, and variant bits.
//!
//! SHA-256 used rather than BLAKE3 or SHA-1 because every language's standard library has it,

extern crate alloc;

use alloc::sync::Arc;
use arrow::array::{
    Array, FixedSizeBinaryArray, FixedSizeBinaryBuilder, StringArray, StringBuilder, UInt8Array,
    UInt8Builder,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{BuildHasherDefault, Hash, Hasher};

use sha2::{Digest, Sha256};

use crate::ephemeris::{frame_pair, recognised};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Namespace prefix mixed into every id, separating this scheme's hash space from any
/// other use of SHA-256 over the same strings.
///
/// These are the first 16 bytes of `SHA-256("soloc.spacetimestamp.prescribed-id.v1")`.
/// The derivation is documented so it can be re-checked, but implementations should paste
/// the constant rather than recompute it — the bytes are the contract, not the phrase.
pub(crate) const NAMESPACE: [u8; 16] = [
    0xee, 0x8c, 0x42, 0x09, 0x0d, 0xd5, 0x9d, 0x8d, 0x14, 0xaf, 0xc9, 0x54, 0x1c, 0xaf, 0xe9, 0x5d,
];

/// Terminal astronomical frame or body: anise almanac resolves it, and chain resolution stops.
pub const KIND_ASTRO: u8 = 0x0;

/// Pose comes from ledger rows: chain resolution recurses into it.
pub const KIND_SOLOC: u8 = 0x1;

/// Only used for source_id
pub const KIND_ABSTRACT: u8 = 0x2;

/// The single reserved authority under which all astronomical names mint.
///
/// This helps solve the special case for astronomical entities, which don't "belong"
/// to any specific user/authority.
pub const ASTRO_AUTHORITY: &str = "astro";

/// Column name for the id in [`registry_schema`].
pub(crate) const REGISTRY_ID_COLUMN: &str = "prescribed_id";

/// Column name for the frame an [`crate::schema::SpaceTimestampBuilder`] row is expressed in.
pub const FRAME_ID_COLUMN: &str = "frame_id";

/// Column name for the source id on a spacetimestamp row
pub const SOURCE_ID_COLUMN: &str = "source_id";

// ---------------------------------------------------------------------------
// PrescribedId
// ---------------------------------------------------------------------------

/// A 16-byte identity derived from `(authority, common_name, kind)`.
///
/// Construct through [`new`](Self::new), [`astronomical`](Self::astronomical), or
/// [`abstract_source`](Self::abstract_source); read raw bytes back with
/// [`from_bytes`](Self::from_bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PrescribedId([u8; 16]);

impl PrescribedId {
    /// Mints a [`KIND_SOLOC`] id
    ///
    /// `authority` is who is speaking (`acme.com`, `norad`, `john`).
    /// It is ASCII-lowercased; `name` is used byte-exact.
    ///
    /// ```
    /// use spacetimestamp::identity::PrescribedId;
    /// let a = PrescribedId::new("acme.com", "truck_A")?;
    /// let b = PrescribedId::new("ACME.COM", "truck_A")?;
    /// assert_eq!(a, b, "authority is case-insensitive");
    /// # Ok::<(), String>(())
    /// ```
    pub fn new(authority: &str, name: &str) -> Result<Self, String> {
        Self::mint(KIND_SOLOC, authority, name)
    }

    /// Mints a [`KIND_ASTRO`] id embedding the anise frame pair `(ephemeris_id,
    /// orientation_id)`.
    ///
    /// Unlike the other kinds, an astro id is not a hash: the two integers are written
    /// directly into the 16 bytes (see [`astro_frame`](Self::astro_frame)), so the id is
    /// self-describing and resolves to a [`Frame`](anise::prelude::Frame) with no name
    /// registry. The pair is validated against the canonical frame table
    /// ([`recognised`](crate::ephemeris::recognised)); an unrecognised pair, including a
    /// nonsense combination like `(399, 499)`, is rejected. Static and offline.
    pub fn astronomical(ephemeris_id: i32, orientation_id: i32) -> Result<Self, String> {
        if !recognised(ephemeris_id, orientation_id) {
            return Err(format!(
                "({ephemeris_id}, {orientation_id}) is not a recognised astronomical frame pair"
            ));
        }
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&ephemeris_id.to_be_bytes());
        bytes[6] = 0x80 | KIND_ASTRO; // version 8 high nibble, KIND_ASTRO low nibble
        bytes[8] = 0x80; // RFC 9562 variant 0b10; no hash bits to preserve
        bytes[9..13].copy_from_slice(&orientation_id.to_be_bytes());
        Ok(Self(bytes))
    }

    /// Mints a [`KIND_ASTRO`] id from a frame *name*, resolving it to its pair via the
    /// canonical table.
    ///
    /// The single home of name-based astro minting: a transform target, a wire request, a
    /// registry label, a test. Names are byte-exact ([`frame_pair`](crate::ephemeris::frame_pair)),
    /// so a typo or wrong case is rejected here rather than minting a wrong id.
    pub fn astronomical_from_name(name: &str) -> Result<Self, String> {
        let (e, o) = frame_pair(name)
            .ok_or_else(|| format!("'{name}' is not a recognised astronomical frame"))?;
        Self::astronomical(e, o)
    }

    /// Mints a [`KIND_ABSTRACT`] id. A source or process that is never a frame.
    ///
    /// Use this for anything that labels provenance rather than occupying space. eg.
    /// an online catelog. This is only used to fill `source_id` in spacetimestamp.
    pub fn abstract_source(authority: &str, name: &str) -> Result<Self, String> {
        Self::mint(KIND_ABSTRACT, authority, name)
    }

    fn mint(kind: u8, authority: &str, name: &str) -> Result<Self, String> {
        let kind = kind & 0x0F;

        if authority.is_empty() {
            return Err("authority must not be empty".to_string());
        }
        if name.is_empty() {
            return Err("name must not be empty".to_string());
        }
        // 0x00 separates the two fields, so allowing it inside either would let
        // ("a\0b", "c") and ("a", "b\0c") mint the same id.
        if authority.contains('\0') {
            return Err(format!(
                "authority '{authority}' must not contain a NUL byte"
            ));
        }
        if name.contains('\0') {
            return Err(format!("name '{name}' must not contain a NUL byte"));
        }

        let authority = authority.to_ascii_lowercase();

        let mut hasher = Sha256::new();
        hasher.update(NAMESPACE);
        hasher.update([kind]);
        hasher.update(authority.as_bytes());
        hasher.update([0x00]);
        hasher.update(name.as_bytes());
        let hash = hasher.finalize();

        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&hash[..16]);
        bytes[6] = 0x80 | kind; // version 8 in the high nibble, kind in the low
        bytes[8] = (bytes[8] & 0x3F) | 0x80; // RFC 9562 variant

        Ok(Self(bytes))
    }

    /// Helper function used to check if raw 16 bytes define valid PrescribedID.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let bytes: [u8; 16] = bytes
            .try_into()
            .map_err(|_| format!("prescribed id must be 16 bytes, got {}", bytes.len()))?;
        if bytes[6] & 0xF0 != 0x80 {
            return Err(format!(
                "not a prescribed id: expected UUID version 8, found version {}",
                bytes[6] >> 4
            ));
        }
        if bytes[8] & 0xC0 != 0x80 {
            return Err("not a prescribed id: RFC 9562 variant bits are not 0b10".to_string());
        }
        Ok(Self(bytes))
    }

    /// The raw 16 bytes, for writing into a `FixedSizeBinary(16)` column.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// The kind nibble: one byte load, mask, compare.
    ///
    /// Callers use it to decide whether to recurse into a
    /// ledger or hand the name to the almanac.
    pub const fn kind(&self) -> u8 {
        self.0[6] & 0x0F
    }

    /// Returns `true` if this id resolves from ledger rows.
    pub const fn is_soloc(&self) -> bool {
        self.kind() == KIND_SOLOC
    }

    /// Returns `true` if this id is a terminal astronomical frame or body.
    pub const fn is_astro(&self) -> bool {
        self.kind() == KIND_ASTRO
    }

    /// Returns `true` if this id is provenance-only and never valid as a frame.
    pub const fn is_abstract(&self) -> bool {
        self.kind() == KIND_ABSTRACT
    }

    /// The embedded anise `(ephemeris_id, orientation_id)` pair, or `None` if not KIND_ASTRO.
    ///
    /// Reads bytes `[0..4]` / `[9..13]` (big-endian) written by
    /// [`astronomical`](Self::astronomical). This is what makes an astro id resolve to a
    /// frame structurally, with no [`NameRegistry`] lookup.
    pub fn astro_frame(&self) -> Option<(i32, i32)> {
        if !self.is_astro() {
            return None;
        }
        let e = i32::from_be_bytes(self.0[0..4].try_into().expect("4 of 16 bytes"));
        let o = i32::from_be_bytes(self.0[9..13].try_into().expect("4 of 16 bytes"));
        Some((e, o))
    }

    /// Formats as a canonical hyphenated UUID (`8-4-4-4-12`).
    ///
    /// This is the display fallback when a [`NameRegistry`] has no entry for the id.
    pub fn to_hyphenated(&self) -> String {
        let h = |r: std::ops::Range<usize>| {
            self.0[r]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        format!(
            "{}-{}-{}-{}-{}",
            h(0..4),
            h(4..6),
            h(6..8),
            h(8..10),
            h(10..16)
        )
    }
}

impl fmt::Display for PrescribedId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hyphenated())
    }
}

/// Hashes as exactly the 16 id bytes and nothing else.
///
/// Written by hand rather than derived because `#[derive(Hash)]` would route through
/// `Hash for [u8; 16]`, which emits a length prefix ahead of the bytes. [`IdHasher`] wants
/// one 16-byte `write` and no other calls, so the prefix would have to be tolerated by every
/// hasher that reads these ids. Behaviour under `SipHash` is unchanged in any way that
/// matters, and `Hash`/`Eq` stay consistent: equal ids hash equal, because equality is
/// byte equality.
impl Hash for PrescribedId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Arrow representation
// ---------------------------------------------------------------------------

/// Arrow field-metadata key naming a canonical extension type.
pub const ARROW_EXTENSION_KEY: &str = "ARROW:extension:name";

/// Arrow field-metadata key carrying an extension type's own payload.
pub const ARROW_EXTENSION_METADATA_KEY: &str = "ARROW:extension:metadata";

/// The canonical Arrow extension name for a 16-byte UUID.
///
/// Set on every id column so a reader that understands the extension renders ids as
/// UUIDs rather than as opaque bytes.
pub const ARROW_UUID_EXTENSION: &str = "arrow.uuid";

/// The storage type of a plain id column: `FixedSizeBinary(16)`.
pub fn id_type() -> DataType {
    DataType::FixedSizeBinary(16)
}

/// Builds a non-nullable plain id field named `name`, annotated as `arrow.uuid`.
///
/// Every identity column in every schema is constructed through this function, so the
/// storage type cannot drift apart between `frame_id`, `source_id`, and a schema's own id
/// column.
pub fn id_field(name: &str) -> Field {
    Field::new(name, id_type(), false).with_metadata(HashMap::from([(
        ARROW_EXTENSION_KEY.to_string(),
        ARROW_UUID_EXTENSION.to_string(),
    )]))
}

/// A builder for a plain id column, pre-allocated for `capacity` rows.
pub fn id_builder(capacity: usize) -> FixedSizeBinaryBuilder {
    FixedSizeBinaryBuilder::with_capacity(capacity, 16)
}

/// Asserts that `arr` is a plain `FixedSizeBinary(16)` id column.
pub fn as_id_column<'a>(
    arr: &'a dyn Array,
    name: &str,
) -> Result<&'a FixedSizeBinaryArray, String> {
    arr.as_any()
        .downcast_ref::<FixedSizeBinaryArray>()
        .filter(|a| a.value_length() == 16)
        .ok_or_else(|| format!("'{name}' is not FixedSizeBinary(16)"))
}

/// Reads the id at `row`, validating it.
///
/// It may be better to validate by direct byte comparison in the future.
/// Panics if `row >= arr.len()`, like `value` itself; callers index within the column.
#[inline]
pub fn id_at(arr: &FixedSizeBinaryArray, row: usize) -> Result<PrescribedId, String> {
    if arr.is_null(row) {
        return Err(format!("id at row {row} is null"));
    }
    PrescribedId::from_bytes(arr.value(row)).map_err(|e| format!("id at row {row}: {e}"))
}

// ---------------------------------------------------------------------------
// Hashing ids
// ---------------------------------------------------------------------------

/// A hasher for ids that are already hashes.
///
/// A [`PrescribedId`] is a truncated SHA-256, so running SipHash over it re-hashes a
/// cryptographic digest. This folds the 16 bytes into a `u64` with two loads and an XOR instead.
///
/// The two halves are XORed rather than one half taken, because both halves carry fixed
/// bits: the version nibble and kind sit in byte 6 (low half) and the variant bits in byte 8
/// (high half). XORing masks each against random bytes from the other half, so all 64 output
/// bits stay full-entropy.
#[derive(Debug, Default, Clone, Copy)]
pub struct IdHasher(u64);

impl Hasher for IdHasher {
    fn write(&mut self, bytes: &[u8]) {
        // The 16-byte path is the only one [`PrescribedId`]'s `Hash` impl takes. Anything
        // else still has to produce *some* hash — a `Hasher` may not panic — so an FNV-1a
        // fold covers it, correct if slower.
        if bytes.len() == 16 {
            let lo = u64::from_le_bytes(bytes[0..8].try_into().expect("8 of 16 bytes"));
            let hi = u64::from_le_bytes(bytes[8..16].try_into().expect("8 of 16 bytes"));
            self.0 ^= lo ^ hi;
        } else {
            for &b in bytes {
                self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
            }
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// A `HashMap` keyed by [`PrescribedId`] using [`IdHasher`].
///
/// Construct with `IdMap::default()` — `HashMap::new` exists only for `RandomState`.
pub type IdMap<V> = HashMap<PrescribedId, V, BuildHasherDefault<IdHasher>>;

/// A `HashSet` of [`PrescribedId`]s using [`IdHasher`]. Construct with `IdSet::default()`.
pub type IdSet = HashSet<PrescribedId, BuildHasherDefault<IdHasher>>;

// ---------------------------------------------------------------------------
// NameRegistry
// ---------------------------------------------------------------------------

/// The Arrow schema for an exported name registry.
///
/// Four plain columns; a registry is expexted to be relatively short (one row per
/// distinct name ever). However, if we have many names, which relates to many entities,
/// we may need to optimize this: not entirely sure how yet.
///
/// `kind` is redundant. it is recoverable from the id's nibble, but it is written anyway
/// so a reader can filter or group without dealing in bits. [`NameRegistry::merge_batch`]
/// checks that it agrees with the id.
pub fn registry_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new(REGISTRY_ID_COLUMN, DataType::FixedSizeBinary(16), false),
        Field::new("authority", DataType::Utf8, false),
        Field::new("common_name", DataType::Utf8, false),
        Field::new("kind", DataType::UInt8, false),
    ]))
}

/// Display names for prescribed ids.
///
/// **This registry is display-only.** Identity, joins, topology, and pose resolution all
/// work on ids and never consult it;
#[derive(Debug, Clone, Default)]
pub struct NameRegistry {
    names: HashMap<PrescribedId, (String, String)>,
}

impl NameRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of registered names.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Returns `true` if no names are registered.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Records `id`'s name, rejecting a binding that does not hash.
    pub fn insert(
        &mut self,
        id: PrescribedId,
        authority: &str,
        common_name: &str,
    ) -> Result<(), String> {
        let expected = expected_binding_id(id.kind(), authority, common_name)?;
        if expected != id {
            return Err(format!(
                "claimed name ('{authority}', '{common_name}') mints {expected}, not {id}"
            ));
        }
        self.names.insert(
            id,
            (authority.to_ascii_lowercase(), common_name.to_string()),
        );
        Ok(())
    }

    /// Returns `(authority, common_name)` for `id`, or `None` if it was never registered.
    pub fn name_of(&self, id: &PrescribedId) -> Option<(&str, &str)> {
        self.names.get(id).map(|(a, n)| (a.as_str(), n.as_str()))
    }

    /// Renders `id` for a human. An astro id derives its name from its embedded frame pair,
    /// needing no registry entry; any other id uses its registered common name, falling back
    /// to the hyphenated id. For display only, no computation over this.
    pub fn display(&self, id: PrescribedId) -> String {
        if let Some(name) = id
            .astro_frame()
            .and_then(|(e, o)| crate::ephemeris::frame_name(e, o))
        {
            return name.to_string();
        }
        self.name_of(&id)
            .map(|(_, name)| name.to_string())
            .unwrap_or_else(|| id.to_hyphenated())
    }

    /// Exports every registered name as a [`registry_schema`] batch.
    ///
    /// Rows are ordered by id currently, so the same registry always exports
    /// byte-identical output, which makes an export diffable.
    pub fn to_batch(&self) -> Result<RecordBatch, String> {
        let mut entries: Vec<_> = self.names.iter().collect();
        entries.sort_by_key(|(id, _)| *id);

        let mut ids = FixedSizeBinaryBuilder::with_capacity(entries.len(), 16);
        let mut authorities = StringBuilder::new();
        let mut names = StringBuilder::new();
        let mut kinds = UInt8Builder::with_capacity(entries.len());

        for (id, (authority, common_name)) in entries {
            ids.append_value(id.as_bytes())
                .map_err(|e| format!("failed to append prescribed id: {e}"))?;
            authorities.append_value(authority);
            names.append_value(common_name);
            kinds.append_value(id.kind());
        }

        RecordBatch::try_new(
            registry_schema(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(authorities.finish()),
                Arc::new(names.finish()),
                Arc::new(kinds.finish()),
            ],
        )
        .map_err(|e| format!("failed to build name registry batch: {e}"))
    }

    /// Merges an exported registry (see [`to_batch`](Self::to_batch)) into this one.
    ///
    /// Every row is verified before anything is applied, so a batch containing one bad
    /// binding leaves this registry untouched rather than half-merged. Returns the number
    /// of names newly learned.
    pub fn merge_batch(&mut self, batch: &RecordBatch) -> Result<usize, String> {
        let verified = verify_registry_batch(batch)?;
        Ok(self.apply_verified(verified))
    }

    /// Merges several exported registry batches **as one unit**.
    ///
    /// Same contract as [`merge_batch`](Self::merge_batch), widened to the whole slice: every
    /// row of every batch is verified before any of them is applied, so a forged binding in
    /// the last batch leaves this registry exactly as it was.
    ///
    /// Calling `merge_batch` in a loop does **not** have that property; it applies each batch
    /// as it goes, so a later failure leaves the leading batches merged while the caller sees
    /// an error. Any multi-batch payload must therefore come through here. May want to combine
    /// this and [`merge_batch`](Self::merge_batch) in the future.
    ///
    /// Returns the number of names newly learned across the whole slice; an id repeated
    /// between batches counts once.
    pub fn merge_batches(&mut self, batches: &[RecordBatch]) -> Result<usize, String> {
        let mut verified = Vec::new();
        for (i, batch) in batches.iter().enumerate() {
            verified.extend(
                verify_registry_batch(batch).map_err(|e| format!("registry batch {i}: {e}"))?,
            );
        }
        Ok(self.apply_verified(verified))
    }

    /// Applies already-verified bindings, returning how many ids were not already present.
    fn apply_verified(&mut self, verified: Vec<(PrescribedId, String, String)>) -> usize {
        let mut learned = 0;
        for (id, authority, common_name) in verified {
            if self.names.insert(id, (authority, common_name)).is_none() {
                learned += 1;
            }
        }
        learned
    }
}

/// The id a registry binding must mint to, given the kind claimed by the id's own nibble.
///
/// An astro id embeds a frame pair rather than hashing a name, so it is verified by resolving
/// the name to that pair and re-embedding; every other kind is verified by re-hashing.
fn expected_binding_id(
    kind: u8,
    authority: &str,
    common_name: &str,
) -> Result<PrescribedId, String> {
    if kind == KIND_ASTRO {
        let (e, o) = frame_pair(common_name)
            .ok_or_else(|| format!("'{common_name}' is not a recognised astronomical frame"))?;
        PrescribedId::astronomical(e, o)
    } else {
        PrescribedId::mint(kind, authority, common_name)
    }
}

/// Verifies every row of a [`registry_schema`] batch, returning the bindings it claims.
///
/// Split out of [`NameRegistry::merge_batch`] so that a whole multi-batch payload can be
/// verified before the first binding is written. This applies nothing itself.
fn verify_registry_batch(
    batch: &RecordBatch,
) -> Result<Vec<(PrescribedId, String, String)>, String> {
    let ids = batch
        .column_by_name(REGISTRY_ID_COLUMN)
        .and_then(|c| c.as_any().downcast_ref::<FixedSizeBinaryArray>())
        .ok_or_else(|| {
            format!("registry batch is missing a FixedSizeBinary(16) '{REGISTRY_ID_COLUMN}' column")
        })?;
    let authorities = batch
        .column_by_name("authority")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| "registry batch is missing a Utf8 'authority' column".to_string())?;
    let names = batch
        .column_by_name("common_name")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| "registry batch is missing a Utf8 'common_name' column".to_string())?;
    let kinds = batch
        .column_by_name("kind")
        .and_then(|c| c.as_any().downcast_ref::<UInt8Array>())
        .ok_or_else(|| "registry batch is missing a UInt8 'kind' column".to_string())?;

    let mut verified = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        let id = PrescribedId::from_bytes(ids.value(row))
            .map_err(|e| format!("registry row {row}: {e}"))?;
        let authority = authorities.value(row);
        let common_name = names.value(row);

        if kinds.value(row) != id.kind() {
            return Err(format!(
                "registry row {row}: kind column says {} but id {id} says {}",
                kinds.value(row),
                id.kind()
            ));
        }
        let expected = expected_binding_id(id.kind(), authority, common_name)
            .map_err(|e| format!("registry row {row}: {e}"))?;
        if expected != id {
            return Err(format!(
                "registry row {row}: claimed name ('{authority}', '{common_name}') \
                 mints {expected}, not {id}"
            ));
        }
        verified.push((id, authority.to_ascii_lowercase(), common_name.to_string()));
    }

    Ok(verified)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(id: &PrescribedId) -> String {
        id.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }

    // -- minting ------------------------------------------------------------

    #[test]
    fn test_mint_is_deterministic_across_calls() {
        let a = PrescribedId::new("acme.com", "truck_A").unwrap();
        let b = PrescribedId::new("acme.com", "truck_A").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn test_distinct_names_mint_distinct_ids() {
        let a = PrescribedId::new("acme.com", "truck_A").unwrap();
        let b = PrescribedId::new("acme.com", "truck_B").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn test_distinct_authorities_mint_distinct_ids() {
        let a = PrescribedId::new("acme.com", "truck_A").unwrap();
        let b = PrescribedId::new("globex.com", "truck_A").unwrap();
        assert_ne!(
            a, b,
            "the same common name under two authorities is two things"
        );
    }

    #[test]
    fn test_uuidv8_version_and_variant_conformance() {
        for id in [
            PrescribedId::new("acme.com", "truck_A").unwrap(),
            PrescribedId::astronomical_from_name("ICRF").unwrap(),
            PrescribedId::abstract_source("acme.com", "pipeline_v3").unwrap(),
        ] {
            let b = id.as_bytes();
            assert_eq!(b[6] & 0xF0, 0x80, "version nibble must be 8: {id}");
            assert_eq!(b[8] & 0xC0, 0x80, "variant bits must be 0b10: {id}");
        }
    }

    #[test]
    fn test_kind_round_trips_through_the_nibble() {
        let soloc = PrescribedId::new("acme.com", "truck_A").unwrap();
        let astro_id = PrescribedId::astronomical_from_name("Earth").unwrap();
        let abs = PrescribedId::abstract_source("acme.com", "pipeline_v3").unwrap();

        assert_eq!(soloc.kind(), KIND_SOLOC);
        assert_eq!(astro_id.kind(), KIND_ASTRO);
        assert_eq!(abs.kind(), KIND_ABSTRACT);

        assert!(soloc.is_soloc() && !soloc.is_astro() && !soloc.is_abstract());
        assert!(astro_id.is_astro() && !astro_id.is_soloc());
        assert!(abs.is_abstract() && !abs.is_soloc());
    }

    #[test]
    fn test_kind_participates_in_the_hash() {
        // Same authority and name, different kind: not just a different nibble, a
        // different body — so flipping the nibble on one cannot forge the other.
        let soloc = PrescribedId::new("acme.com", "thing").unwrap();
        let abs = PrescribedId::abstract_source("acme.com", "thing").unwrap();

        let mut forged = *soloc.as_bytes();
        forged[6] = 0x80 | KIND_ABSTRACT;
        assert_ne!(&forged, abs.as_bytes(), "nibble flip must not forge a kind");
    }

    #[test]
    fn test_reserved_astro_authority_cannot_be_squatted() {
        // An operator who picks "astro" as their own authority mints under KIND_SOLOC,
        // so the reserved namespace needs no policing.
        let squatter = PrescribedId::new(ASTRO_AUTHORITY, "ICRF").unwrap();
        let real = PrescribedId::astronomical_from_name("ICRF").unwrap();
        assert_ne!(squatter, real);
        assert_eq!(squatter.kind(), KIND_SOLOC);
        assert_eq!(real.kind(), KIND_ASTRO);
    }

    // -- normalization ------------------------------------------------------

    #[test]
    fn test_authority_is_case_folded() {
        let lower = PrescribedId::new("acme.com", "truck_A").unwrap();
        for variant in ["ACME.COM", "Acme.Com", "aCmE.cOm"] {
            assert_eq!(PrescribedId::new(variant, "truck_A").unwrap(), lower);
        }
    }

    #[test]
    fn test_name_is_case_sensitive() {
        // anise resolves `Earth` but not `EARTH`, so folding names would make valid
        // frames unresolvable. Names are hashed byte-exact.
        let a = PrescribedId::new("acme.com", "truck_A").unwrap();
        let b = PrescribedId::new("acme.com", "TRUCK_A").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn test_registry_stores_the_folded_authority() {
        let id = PrescribedId::new("ACME.COM", "truck_A").unwrap();
        let mut reg = NameRegistry::new();
        reg.insert(id, "ACME.COM", "truck_A").unwrap();
        assert_eq!(reg.name_of(&id), Some(("acme.com", "truck_A")));
    }

    // -- input rejection ----------------------------------------------------

    #[test]
    fn test_empty_fields_are_rejected() {
        assert!(PrescribedId::new("", "truck_A").is_err());
        assert!(PrescribedId::new("acme.com", "").is_err());
        assert!(PrescribedId::abstract_source("", "x").is_err());
    }

    #[test]
    fn test_nul_bytes_are_rejected() {
        // Without this, ("a\0b", "c") and ("a", "b\0c") would hash the same bytes.
        assert!(PrescribedId::new("acme\0com", "truck").is_err());
        assert!(PrescribedId::new("acme.com", "truck\0A").is_err());
    }

    // -- astronomical validation --------------------------------------------

    #[test]
    fn test_astronomical_accepts_recognised_pairs() {
        // A reference frame, a body-fixed body, and an inertial minor moon.
        for (e, o) in [(0, 1), (399, 399), (606, 1)] {
            assert!(
                PrescribedId::astronomical(e, o).is_ok(),
                "({e}, {o}) should mint"
            );
        }
        // And they round-trip through the embedded bytes.
        assert_eq!(
            PrescribedId::astronomical(399, 399).unwrap().astro_frame(),
            Some((399, 399))
        );
    }

    #[test]
    fn test_astronomical_rejects_an_unrecognised_pair() {
        // (Earth ephemeris, Mars orientation): both components exist, the pair does not.
        let err = PrescribedId::astronomical(399, 499).unwrap_err();
        assert!(
            err.contains("399") && err.contains("499"),
            "error should name the bad pair: {err}"
        );
        // A body-fixed frame for a body that has no PCK model is equally unrecognised.
        assert!(PrescribedId::astronomical(2099942, 2099942).is_err());
    }

    // -- byte round-trip ----------------------------------------------------

    #[test]
    fn test_from_bytes_round_trip() {
        let id = PrescribedId::new("acme.com", "truck_A").unwrap();
        assert_eq!(PrescribedId::from_bytes(id.as_bytes()).unwrap(), id);
    }

    #[test]
    fn test_from_bytes_rejects_bad_input() {
        let id = PrescribedId::new("acme.com", "truck_A").unwrap();

        assert!(PrescribedId::from_bytes(&[0u8; 15]).is_err(), "short");
        assert!(PrescribedId::from_bytes(&[0u8; 17]).is_err(), "long");

        let mut bad_version = *id.as_bytes();
        bad_version[6] = 0x40 | KIND_SOLOC;
        assert!(PrescribedId::from_bytes(&bad_version).is_err(), "version 4");

        let mut bad_variant = *id.as_bytes();
        bad_variant[8] &= 0x3F;
        assert!(PrescribedId::from_bytes(&bad_variant).is_err(), "variant");
    }

    #[test]
    fn test_from_bytes_accepts_an_unknown_kind() {
        // Forward compatibility: an id minted by a future version with a kind this build
        // does not know is still a valid identity to store, join on, and display.
        let mut future = *PrescribedId::new("acme.com", "truck_A").unwrap().as_bytes();
        future[6] = 0x80 | 0x07;
        let id = PrescribedId::from_bytes(&future).expect("unknown kind should parse");
        assert_eq!(id.kind(), 0x07);
        assert!(!id.is_soloc() && !id.is_astro() && !id.is_abstract());
    }

    #[test]
    fn test_hyphenated_format() {
        let id = PrescribedId::new("acme.com", "truck_A").unwrap();
        let s = id.to_hyphenated();

        let parts: Vec<&str> = s.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(s.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_eq!(s, id.to_string(), "Display must match to_hyphenated");
        assert!(parts[2].starts_with('8'), "version nibble is visible: {s}");
    }

    // -- registry -----------------------------------------------------------

    #[test]
    fn test_registry_insert_and_lookup() {
        let id = PrescribedId::new("acme.com", "truck_A").unwrap();
        let mut reg = NameRegistry::new();
        assert!(reg.is_empty());

        reg.insert(id, "acme.com", "truck_A").unwrap();
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.name_of(&id), Some(("acme.com", "truck_A")));
    }

    #[test]
    fn test_registry_lookup_misses_are_none() {
        let reg = NameRegistry::new();
        let id = PrescribedId::new("acme.com", "truck_A").unwrap();
        assert_eq!(reg.name_of(&id), None, "an unregistered id is harmless");
    }

    #[test]
    fn test_registry_rejects_a_false_binding() {
        let id = PrescribedId::new("acme.com", "truck_A").unwrap();
        let mut reg = NameRegistry::new();

        let err = reg.insert(id, "acme.com", "truck_B").unwrap_err();
        assert!(
            err.contains("truck_B"),
            "error should name the claim: {err}"
        );
        assert!(reg.is_empty(), "a rejected binding must not be stored");
    }

    #[test]
    fn test_registry_batch_round_trip() {
        let mut source = NameRegistry::new();
        let truck = PrescribedId::new("acme.com", "truck_A").unwrap();
        let icrf = PrescribedId::astronomical_from_name("ICRF").unwrap();
        let pipe = PrescribedId::abstract_source("acme.com", "pipeline_v3").unwrap();
        source.insert(truck, "acme.com", "truck_A").unwrap();
        source.insert(icrf, ASTRO_AUTHORITY, "ICRF").unwrap();
        source.insert(pipe, "acme.com", "pipeline_v3").unwrap();

        let batch = source.to_batch().unwrap();
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.schema(), registry_schema());

        let mut target = NameRegistry::new();
        assert_eq!(target.merge_batch(&batch).unwrap(), 3);
        assert_eq!(target.name_of(&truck), Some(("acme.com", "truck_A")));
        assert_eq!(target.name_of(&icrf), Some((ASTRO_AUTHORITY, "ICRF")));
        assert_eq!(target.name_of(&pipe), Some(("acme.com", "pipeline_v3")));
    }

    #[test]
    fn test_registry_export_is_deterministic() {
        // Insertion order must not leak into the export, or a round-trip test proves
        // nothing and two exports of the same registry fail to diff.
        let ids = [
            (
                PrescribedId::new("acme.com", "truck_A").unwrap(),
                "acme.com",
                "truck_A",
            ),
            (
                PrescribedId::new("acme.com", "truck_B").unwrap(),
                "acme.com",
                "truck_B",
            ),
            (
                PrescribedId::astronomical_from_name("ICRF").unwrap(),
                ASTRO_AUTHORITY,
                "ICRF",
            ),
        ];

        let mut forward = NameRegistry::new();
        for (id, a, n) in ids {
            forward.insert(id, a, n).unwrap();
        }
        let mut backward = NameRegistry::new();
        for (id, a, n) in ids.iter().rev() {
            backward.insert(*id, a, n).unwrap();
        }

        assert_eq!(forward.to_batch().unwrap(), backward.to_batch().unwrap());
    }

    #[test]
    fn test_registry_merge_is_idempotent() {
        let mut source = NameRegistry::new();
        let id = PrescribedId::new("acme.com", "truck_A").unwrap();
        source.insert(id, "acme.com", "truck_A").unwrap();
        let batch = source.to_batch().unwrap();

        let mut target = NameRegistry::new();
        assert_eq!(target.merge_batch(&batch).unwrap(), 1);
        assert_eq!(target.merge_batch(&batch).unwrap(), 0, "no new names");
        assert_eq!(target.len(), 1);
    }

    #[test]
    fn test_registry_merge_rejects_a_tampered_batch_wholesale() {
        // A peer claims one honest name and one it cannot possibly own.
        let honest = PrescribedId::new("acme.com", "truck_A").unwrap();
        let stolen = PrescribedId::new("acme.com", "truck_B").unwrap();

        let mut ids = FixedSizeBinaryBuilder::with_capacity(2, 16);
        ids.append_value(honest.as_bytes()).unwrap();
        ids.append_value(stolen.as_bytes()).unwrap();
        let mut authorities = StringBuilder::new();
        authorities.append_value("acme.com");
        authorities.append_value("acme.com");
        let mut names = StringBuilder::new();
        names.append_value("truck_A");
        names.append_value("a_completely_different_thing");
        let mut kinds = UInt8Builder::new();
        kinds.append_value(KIND_SOLOC);
        kinds.append_value(KIND_SOLOC);

        let batch = RecordBatch::try_new(
            registry_schema(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(authorities.finish()),
                Arc::new(names.finish()),
                Arc::new(kinds.finish()),
            ],
        )
        .unwrap();

        let mut target = NameRegistry::new();
        let err = target.merge_batch(&batch).unwrap_err();
        assert!(
            err.contains("row 1"),
            "error should locate the bad row: {err}"
        );
        assert!(
            target.is_empty(),
            "one bad binding must leave the registry untouched, not half-merged"
        );
    }

    #[test]
    fn test_merge_batches_applies_nothing_when_a_later_batch_is_forged() {
        // Batch 0 is honest, batch 1 relabels an id it cannot own. Merging batch by batch
        // would leave batch 0 applied while the caller sees an error; merging the payload as
        // one unit must leave nothing behind.
        let honest = PrescribedId::new("acme.com", "truck_A").unwrap();
        let mut source = NameRegistry::new();
        source.insert(honest, "acme.com", "truck_A").unwrap();
        let good = source.to_batch().unwrap();

        let forged = RecordBatch::try_new(
            registry_schema(),
            vec![
                good.column(0).clone(),
                good.column(1).clone(),
                Arc::new(StringArray::from(vec!["impostor"])),
                good.column(3).clone(),
            ],
        )
        .unwrap();

        let mut target = NameRegistry::new();
        let err = target.merge_batches(&[good, forged]).unwrap_err();
        assert!(
            err.contains("registry batch 1"),
            "error should locate the bad batch: {err}"
        );
        assert!(
            target.is_empty(),
            "a leading batch must not survive a later batch's rejection"
        );
    }

    #[test]
    fn test_merge_batches_learns_the_union_and_counts_a_repeat_once() {
        let truck = PrescribedId::new("acme.com", "truck_A").unwrap();
        let icrf = PrescribedId::astronomical_from_name("ICRF").unwrap();

        let mut first = NameRegistry::new();
        first.insert(truck, "acme.com", "truck_A").unwrap();
        let mut second = NameRegistry::new();
        second.insert(truck, "acme.com", "truck_A").unwrap();
        second.insert(icrf, ASTRO_AUTHORITY, "ICRF").unwrap();

        let mut target = NameRegistry::new();
        let learned = target
            .merge_batches(&[first.to_batch().unwrap(), second.to_batch().unwrap()])
            .unwrap();
        assert_eq!(learned, 2, "truck_A is in both batches and counts once");
        assert_eq!(target.len(), 2);
    }

    #[test]
    fn test_registry_merge_rejects_a_mismatched_kind_column() {
        let id = PrescribedId::new("acme.com", "truck_A").unwrap();
        let mut ids = FixedSizeBinaryBuilder::with_capacity(1, 16);
        ids.append_value(id.as_bytes()).unwrap();
        let mut authorities = StringBuilder::new();
        authorities.append_value("acme.com");
        let mut names = StringBuilder::new();
        names.append_value("truck_A");
        let mut kinds = UInt8Builder::new();
        kinds.append_value(KIND_ASTRO); // lies: the id nibble says KIND_SOLOC

        let batch = RecordBatch::try_new(
            registry_schema(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(authorities.finish()),
                Arc::new(names.finish()),
                Arc::new(kinds.finish()),
            ],
        )
        .unwrap();

        let mut target = NameRegistry::new();
        assert!(target.merge_batch(&batch).is_err());
        assert!(target.is_empty());
    }

    #[test]
    fn test_registry_merge_rejects_a_wrong_schema() {
        let batch = crate::topology::TransformTree::new()
            .to_log_batch()
            .unwrap();
        let mut reg = NameRegistry::new();
        let err = reg.merge_batch(&batch).unwrap_err();
        assert!(
            err.contains(REGISTRY_ID_COLUMN),
            "should name the column: {err}"
        );
    }

    // -- cross-language contract --------------------------------------------

    #[test]
    fn frozen_mint_vectors() {
        // THE CROSS-LANGUAGE CONTRACT. These bytes are pinned: the pyarrow client and the
        // visualizer must reproduce them exactly. if this test fails,
        // minting has changed, and every client must ship a matching change
        // or federation silently splits in two.
        //
        // Reference implementation, for a port to check itself against. KIND_SOLOC and
        // KIND_ABSTRACT hash; KIND_ASTRO embeds the anise frame pair instead:
        //
        //   def mint(kind: int, authority: str, name: str) -> bytes:   # SOLOC / ABSTRACT
        //       h = hashlib.sha256()
        //       h.update(NAMESPACE)                          # the 16 bytes below
        //       h.update(bytes([kind]))
        //       h.update(authority.lower().encode())         # ASCII fold only
        //       h.update(b"\x00")
        //       h.update(name.encode())                      # byte-exact, no folding
        //       b = bytearray(h.digest()[:16])
        //       b[6] = 0x80 | kind
        //       b[8] = (b[8] & 0x3F) | 0x80
        //       return bytes(b)
        //
        //   def astronomical(ephemeris_id: int, orientation_id: int) -> bytes:  # ASTRO
        //       b = bytearray(16)
        //       b[0:4]  = ephemeris_id.to_bytes(4, "big", signed=True)
        //       b[6]    = 0x80                               # version 8 | KIND_ASTRO 0x0
        //       b[8]    = 0x80                               # RFC 9562 variant 0b10
        //       b[9:13] = orientation_id.to_bytes(4, "big", signed=True)
        //       return bytes(b)

        let namespace_hex: String = NAMESPACE.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            namespace_hex, "ee8c42090dd59d8d14afc9541cafe95d",
            "the NAMESPACE constant is itself part of the contract"
        );

        let cases: &[(PrescribedId, &str)] = &[
            // KIND_SOLOC — ledger-resolved entities (hashed)
            (
                PrescribedId::new("acme.com", "truck_A").unwrap(),
                "70591c8b9b2b81edb6314748930a1713",
            ),
            (
                PrescribedId::new("acme.com", "cam").unwrap(),
                "f5d55ca2261a81a0965c1100b928f306",
            ),
            // KIND_ASTRO — anise frame pair (ephemeris_id, orientation_id) embedded, not hashed.
            // ICRF (0, 1) reference; Earth (399, 399) body-fixed; EME2000 (399, 1) inertial;
            // Titan (606, 1) inertial minor moon.
            (
                PrescribedId::astronomical(0, 1).unwrap(),
                "00000000000080008000000001000000",
            ),
            (
                PrescribedId::astronomical(399, 399).unwrap(),
                "0000018f00008000800000018f000000",
            ),
            (
                PrescribedId::astronomical(399, 1).unwrap(),
                "0000018f000080008000000001000000",
            ),
            (
                PrescribedId::astronomical(606, 1).unwrap(),
                "0000025e000080008000000001000000",
            ),
            // KIND_ABSTRACT — provenance only (hashed)
            (
                PrescribedId::abstract_source("acme.com", "pipeline_v3").unwrap(),
                "6af4052e183f82d2b617092fb6da6fa5",
            ),
        ];

        for (id, expected) in cases {
            assert_eq!(&hex(id), expected, "mint recipe changed for {id}");
        }

        // Decision B, pinned: a bare body name and its IAU_ spelling are one id.
        assert_eq!(
            PrescribedId::astronomical_from_name("Earth").unwrap(),
            PrescribedId::astronomical_from_name("IAU_EARTH").unwrap()
        );
    }

    #[test]
    fn frozen_vectors_are_uuid_conformant() {
        // Guards the vectors themselves: a pinned constant that is not a legal UUID would
        // freeze a bug into every client that copies it.
        for (id, kind) in [
            (
                PrescribedId::new("acme.com", "truck_A").unwrap(),
                KIND_SOLOC,
            ),
            (
                PrescribedId::astronomical_from_name("ICRF").unwrap(),
                KIND_ASTRO,
            ),
            (
                PrescribedId::abstract_source("acme.com", "pipeline_v3").unwrap(),
                KIND_ABSTRACT,
            ),
        ] {
            let s = id.to_hyphenated();
            let groups: Vec<&str> = s.split('-').collect();
            assert!(
                groups[2].starts_with('8'),
                "version nibble must read 8 in {s}"
            );
            assert_eq!(
                groups[2].as_bytes()[1],
                char::from_digit(u32::from(kind), 16).unwrap() as u8,
                "kind nibble must read {kind:x} in {s}"
            );
            assert!(
                matches!(groups[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'),
                "variant must read 8/9/a/b in {s}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Plain id columns
    // -----------------------------------------------------------------------

    /// A spread of distinct ids across all three kinds, for the column round-trip tests.
    fn sample_ids() -> Vec<PrescribedId> {
        vec![
            PrescribedId::new("acme.com", "truck_A").unwrap(),
            PrescribedId::new("acme.com", "cam").unwrap(),
            PrescribedId::astronomical_from_name("ICRF").unwrap(),
            PrescribedId::astronomical_from_name("Earth").unwrap(),
            PrescribedId::astronomical_from_name("Mars").unwrap(),
            PrescribedId::abstract_source("acme.com", "gps").unwrap(),
        ]
    }

    fn id_array(ids: &[PrescribedId]) -> FixedSizeBinaryArray {
        let mut b = id_builder(ids.len());
        for id in ids {
            b.append_value(id.as_bytes()).unwrap();
        }
        b.finish()
    }

    #[test]
    fn id_field_carries_the_uuid_extension() {
        let f = id_field(FRAME_ID_COLUMN);
        assert_eq!(f.data_type(), &DataType::FixedSizeBinary(16));
        assert!(!f.is_nullable(), "a row without an identity is not a row");
        assert_eq!(
            f.metadata().get(ARROW_EXTENSION_KEY).map(String::as_str),
            Some(ARROW_UUID_EXTENSION)
        );
    }

    #[test]
    fn id_column_round_trips_through_the_builder() {
        let ids = sample_ids();
        let arr = id_array(&ids);
        assert_eq!(arr.len(), ids.len());
        for (i, want) in ids.iter().enumerate() {
            assert_eq!(id_at(&arr, i).unwrap(), *want);
        }
    }

    #[test]
    fn as_id_column_accepts_a_plain_id_column_and_rejects_others() {
        let arr = id_array(&sample_ids());
        assert!(as_id_column(&arr, FRAME_ID_COLUMN).is_ok());

        // Wrong width: FixedSizeBinary, but not 16 bytes.
        let mut narrow = FixedSizeBinaryBuilder::with_capacity(1, 8);
        narrow.append_value([0u8; 8]).unwrap();
        let narrow = narrow.finish();
        let err = as_id_column(&narrow, FRAME_ID_COLUMN).unwrap_err();
        assert!(
            err.contains(FRAME_ID_COLUMN),
            "message names the column: {err}"
        );

        // Wrong type entirely.
        let strings = StringArray::from(vec!["truck_A"]);
        assert!(as_id_column(&strings, SOURCE_ID_COLUMN).is_err());
    }

    #[test]
    fn id_at_rejects_corrupt_and_null_slots() {
        let good = PrescribedId::new("acme.com", "truck_A").unwrap();

        // Version nibble clobbered: byte 6 high nibble must be 0x8.
        let mut bad_version = *good.as_bytes();
        bad_version[6] = 0x40 | (bad_version[6] & 0x0F);

        // Variant bits clobbered: byte 8 high two bits must be 0b10.
        let mut bad_variant = *good.as_bytes();
        bad_variant[8] &= 0x3F;

        let mut b = id_builder(3);
        b.append_value(good.as_bytes()).unwrap();
        b.append_value(bad_version).unwrap();
        b.append_value(bad_variant).unwrap();
        b.append_null();
        let arr = b.finish();

        assert_eq!(id_at(&arr, 0).unwrap(), good);
        for row in [1usize, 2] {
            let err = id_at(&arr, row).unwrap_err();
            assert!(err.contains(&format!("row {row}")), "row is named: {err}");
        }
        let err = id_at(&arr, 3).unwrap_err();
        assert!(
            err.contains("null"),
            "null slot is rejected, not read: {err}"
        );
    }

    #[test]
    fn id_hasher_distributes_across_the_frozen_vectors() {
        use std::hash::BuildHasher;

        let build = BuildHasherDefault::<IdHasher>::default();

        // The frozen vectors: distinct ids hash distinctly, equal ids hash equally.
        let ids = sample_ids();
        let hashes: Vec<u64> = ids.iter().map(|id| build.hash_one(id)).collect();
        for (i, a) in hashes.iter().enumerate() {
            for (j, b) in hashes.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "{} and {} collide", ids[i], ids[j]);
                }
            }
        }
        assert_eq!(build.hash_one(ids[0]), build.hash_one(ids[0]));

        // Bit coverage needs a real sample: across six ids a given output bit is unset in
        // all of them with probability 2^-6, so roughly one bit of 64 would be missing by
        // chance alone. At 512 ids that is 2^-512.
        let many: Vec<PrescribedId> = (0..512)
            .map(|i| PrescribedId::new("acme.com", &format!("entity_{i}")).unwrap())
            .collect();
        let many_hashes: Vec<u64> = many.iter().map(|id| build.hash_one(id)).collect();

        // Both halves reach the output: the fixed version/kind nibble (byte 6) and variant
        // bits (byte 8) must not pin any output bit.
        let ones = many_hashes.iter().fold(0u64, |acc, h| acc | h);
        let zeros = many_hashes.iter().fold(!0u64, |acc, h| acc & h);
        assert_eq!(ones, !0, "some bit is never set across the sample");
        assert_eq!(zeros, 0, "some bit is always set across the sample");

        let distinct: HashSet<u64> = many_hashes.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            many_hashes.len(),
            "folding 128 bits into 64 should not collide at this sample size"
        );
    }

    #[test]
    fn id_maps_and_sets_key_by_identity() {
        let ids = sample_ids();
        let mut map: IdMap<usize> = IdMap::default();
        for (i, id) in ids.iter().enumerate() {
            map.insert(*id, i);
        }
        assert_eq!(map.len(), ids.len());
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(map.get(id), Some(&i));
        }
        // Re-minting the same id finds the same entry — the whole point of a prescribed id.
        assert_eq!(
            map.get(&PrescribedId::new("acme.com", "cam").unwrap()),
            Some(&1)
        );

        let set: IdSet = ids.iter().copied().collect();
        assert_eq!(set.len(), ids.len());
        assert!(set.contains(&PrescribedId::astronomical_from_name("ICRF").unwrap()));
    }
}
