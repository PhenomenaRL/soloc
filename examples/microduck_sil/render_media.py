"""Renders the 3D viewer's views for docs/img: a still (PNG) or, with --to, an animated WebP from the
view's time to --to. Views are the lines the viewer's C key copies. The page is driven frame by
frame in headless Chromium over the DevTools protocol (`stepTo` in the page), at 2× pixel density,
then scaled down to --out-width (smoother edges than rendering at that size).

    ../sim_study_0/.venv/bin/python render_media.py 'view: {...}' --out docs/img/spotting.png
    ../sim_study_0/.venv/bin/python render_media.py 'view: {...}' --to 190 --out docs/img/spotting.webp
    ../sim_study_0/.venv/bin/python render_media.py 'view: {...}' 'view: {..., "at": 5}' --out zoom.webp

Needs the exported page (`export_viewer.py`), ffmpeg for WebP, and Playwright's headless shell.
"""

import argparse
import asyncio
import base64
import json
import math
import re
import subprocess
import tempfile
import urllib.parse
import urllib.request
from pathlib import Path

import websockets

HERE = Path(__file__).resolve().parent
PAGE = HERE / "out" / "microduck_aligned_3d.html"
CHROME = ".cache/ms-playwright/chromium_headless_shell-*/*/chrome-headless-shell"   # under $HOME
LEAD_IN_S = 1.0                     # camera easing settles over this much sim time before frame 1


class Page:
    """One Chromium tab, spoken to over the DevTools protocol."""

    def __init__(self, ws):
        self.ws, self.n, self.events = ws, 0, []

    async def call(self, method, **params):
        self.n += 1
        await self.ws.send(json.dumps({"id": self.n, "method": method, "params": params}))
        while True:
            msg = json.loads(await self.ws.recv())
            if msg.get("id") == self.n:
                if "error" in msg:
                    raise RuntimeError(f"{method}: {msg['error']}")
                return msg.get("result", {})
            self.events.append(msg.get("method"))

    async def js(self, expr):
        r = await self.call("Runtime.evaluate", expression=expr, awaitPromise=True, returnByValue=True)
        if "exceptionDetails" in r:
            raise RuntimeError(f"{expr}: {r['exceptionDetails']}")
        return r["result"].get("value")

    async def frame(self, t, dt, cam=None, toggles=None) -> bytes:
        await self.js(f"stepTo({t}, {dt}, {json.dumps(cam)}, {json.dumps(toggles)}); "
                      "new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))")
        return base64.b64decode((await self.call("Page.captureScreenshot", format="png"))["data"])


TOGGLES = ("trail", "labels", "rays", "raysOwn", "bldg", "fov")   # the page's VIEW_TOGGLES


def plan(views: list[dict], args) -> list[tuple[float, dict | None, dict | None]]:
    """Per frame, (sim time, orbit camera, toggles). One view: a still, or with --to the camera held
    while time runs at --speed. Several: keyframes at their "at" (animation s; the first at 0), the
    camera eased between them (distance on a log scale), sim time linear, and each keyframe's
    toggles from the frame after it is reached."""
    if len(views) == 1:
        t0 = float(views[0]["T"])
        if args.to is None:
            return [(t0, None, None)]
        n = int(round((args.to - t0) * args.fps / args.speed)) + 1
        return [(t0 + i * args.speed / args.fps, None, None) for i in range(n)]
    keys = [{**v, "at": float(v.get("at", 0))} for v in views]
    if any(k["focus"] != keys[0]["focus"] for k in keys) or any(b["at"] <= a["at"] for a, b in zip(keys, keys[1:])):
        raise SystemExit("keyframes need one focus and increasing \"at\"")
    out = []
    for i in range(int(round(keys[-1]["at"] * args.fps)) + 1):
        a = min(i / args.fps, keys[-1]["at"])
        k = next(j for j in range(len(keys) - 1) if a <= keys[j + 1]["at"])
        p, q = keys[k], keys[k + 1]
        s = (a - p["at"]) / (q["at"] - p["at"])
        e = s * s * (3 - 2 * s)
        cam = {"az": p["az"] + (q["az"] - p["az"]) * e, "el": p["el"] + (q["el"] - p["el"]) * e,
               "dist": math.exp(math.log(p["dist"]) + (math.log(q["dist"]) - math.log(p["dist"])) * e)}
        out.append((p["T"] + (q["T"] - p["T"]) * s, cam, {t: p[t] for t in TOGGLES if t in p}))
    return out


async def render(view: dict, frames_plan: list, args, frames_dir: Path) -> int:
    chrome = sorted(Path.home().glob(CHROME))[-1]
    proc = await asyncio.create_subprocess_exec(
        str(chrome), "--headless", "--enable-unsafe-swiftshader", "--use-angle=swiftshader",
        "--remote-debugging-port=0", f"--window-size={args.width},{args.height}", "about:blank",
        stderr=asyncio.subprocess.PIPE)
    try:
        while not (m := re.search(rb"ws://127\.0\.0\.1:(\d+)/", await proc.stderr.readline())):
            pass
        tabs = json.load(urllib.request.urlopen(f"http://127.0.0.1:{m[1].decode()}/json/list"))
        url = next(t["webSocketDebuggerUrl"] for t in tabs if t["type"] == "page")
        async with websockets.connect(url, max_size=None) as ws:
            page = Page(ws)
            await page.call("Emulation.setDeviceMetricsOverride", width=args.width, height=args.height,
                            deviceScaleFactor=args.scale, mobile=False)
            await page.call("Page.navigate", url=f"{args.page.resolve().as_uri()}#view={urllib.parse.quote(json.dumps(view))}")
            for _ in range(600):
                if await page.js("typeof stepTo === 'function' && document.readyState === 'complete'"):
                    break
                await asyncio.sleep(0.2)
            n = len(frames_plan)
            if n == 1:
                t, cam, tog = frames_plan[0]
                (frames_dir / "f0000.png").write_bytes(await page.frame(t, 0, cam, tog))
                return 1
            t0, cam0, tog0 = frames_plan[0]
            step = max(frames_plan[1][0] - t0, 1e-3)    # sim seconds per frame, for the camera easing
            for i in range(int(LEAD_IN_S * args.fps), 0, -1):
                await page.frame(t0 - i * step, step, cam0, tog0)
            for i, (t, cam, tog) in enumerate(frames_plan):
                dt = step if i == 0 else t - frames_plan[i - 1][0]
                (frames_dir / f"f{i:04d}.png").write_bytes(await page.frame(t, dt, cam, tog))
                print(f"\r  frame {i + 1}/{n}", end="", flush=True)
            print()
            return n
    finally:
        proc.terminate()
        await proc.wait()


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("views", nargs="+", help="lines the viewer's C key copies: 'view: {...}'; several are "
                   "keyframes, each after the first with \"at\": animation seconds")
    p.add_argument("--out", type=Path, required=True, help=".png for a still, .webp for an animation")
    p.add_argument("--to", type=float, help="animate from the view's time to this sim time (s)")
    p.add_argument("--fps", type=float, default=15)
    p.add_argument("--speed", type=float, default=1.0, help="sim seconds per second of animation")
    p.add_argument("--width", type=int, default=1200, help="CSS pixels; the image is --scale times this")
    p.add_argument("--height", type=int, default=760)
    p.add_argument("--scale", type=float, default=2)
    p.add_argument("--out-width", type=int, default=1200, help="output pixels wide, scaled down from the render (0: as rendered)")
    p.add_argument("--quality", type=int, default=80, help="WebP quality, 0–100")
    p.add_argument("--page", type=Path, default=PAGE)
    args = p.parse_args()
    views = [json.loads(re.sub(r"^\s*view:\s*", "", v)) for v in args.views]
    frames_plan = plan(views, args)
    with tempfile.TemporaryDirectory() as tmp:
        frames = Path(tmp)
        n = asyncio.run(render(views[0], frames_plan, args, frames))
        args.out.parent.mkdir(parents=True, exist_ok=True)
        scale = ["-vf", f"scale={args.out_width}:-2:flags=lanczos"] if args.out_width else []
        if n == 1:
            subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-i", str(frames / "f0000.png"), *scale, str(args.out)], check=True)
        else:
            subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-framerate", str(args.fps), "-i", str(frames / "f%04d.png"),
                            *scale, "-c:v", "libwebp_anim", "-q:v", str(args.quality), "-loop", "0", str(args.out)], check=True)
    print(f"{args.out}  ({args.out.stat().st_size / 1e6:.1f} MB, {n} frame{'s' if n > 1 else ''})")


if __name__ == "__main__":
    main()
