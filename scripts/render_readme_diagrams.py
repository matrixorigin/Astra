# -*- coding: utf-8 -*-
"""Render the README architecture diagrams to light and dark SVG.

The README embeds these through <picture> elements so each figure follows the
reader's color scheme. Edit this file rather than the generated SVG, then run:

    python3 scripts/render_readme_diagrams.py

and commit the regenerated files under docs/assets/diagrams/. The script has no
dependencies beyond the standard library.
"""
import os, html

OUT = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                   "docs", "assets", "diagrams")

LIGHT = dict(
    text="#1f2328", muted="#59636e", line="#8c959f",
    surface="#ffffff", panel="#f6f8fa",
    b1="#0969da", b1f="#ddf4ff",   # blue   - intelligence
    b2="#8250df", b2f="#fbefff",   # purple - context
    b3="#bf8700", b3f="#fff8c5",   # amber  - policy
    b4="#1a7f37", b4f="#dafbe1",   # green  - execution
    b5="#59636e", b5f="#f6f8fa",   # gray   - evidence
    b6="#cf222e", b6f="#ffebe9",   # red    - terminal/failure
)
DARK = dict(
    text="#e6edf3", muted="#9198a1", line="#6e7681",
    surface="#0d1117", panel="#161b22",
    b1="#4493f8", b1f="#121d2f",
    b2="#a371f7", b2f="#1d1b2e",
    b3="#d29922", b3f="#2a2213",
    b4="#3fb950", b4f="#12261e",
    b5="#9198a1", b5f="#161b22",
    b6="#f85149", b6f="#2b1416",
)

def w_of(s, size, bold=False):
    per = size * (0.575 if bold else 0.545)
    return len(s) * per

def esc(s):
    return html.escape(s, quote=False)

class Svg:
    def __init__(self, w, h, c):
        self.w, self.h, self.c = w, h, c
        self.parts = []

    def defs(self):
        c = self.c
        return (
            '<defs>'
            f'<marker id="a" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">'
            f'<path d="M0 0 L10 5 L0 10 z" fill="{c["line"]}"/></marker>'
            f'<marker id="b" viewBox="0 0 10 10" refX="1" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">'
            f'<path d="M0 0 L10 5 L0 10 z" fill="{c["line"]}"/></marker>'
            '</defs>'
        )

    def box(self, x, y, w, h, title, sub=None, tone="b5", ts=13, ss=11):
        c = self.c
        stroke, fill = c[tone], c[tone + "f"]
        cx = x + w / 2
        self.parts.append(
            f'<rect x="{x:.1f}" y="{y:.1f}" width="{w:.1f}" height="{h:.1f}" rx="7" '
            f'fill="{fill}" stroke="{stroke}" stroke-width="1.4"/>'
        )
        if sub:
            ty = y + h / 2 - 5
            self.parts.append(
                f'<text x="{cx:.1f}" y="{ty:.1f}" text-anchor="middle" font-family="system-ui,-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif" '
                f'font-size="{ts}" font-weight="600" fill="{c["text"]}">{esc(title)}</text>'
                f'<text x="{cx:.1f}" y="{ty + 16:.1f}" text-anchor="middle" font-family="system-ui,-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif" '
                f'font-size="{ss}" fill="{c["muted"]}">{esc(sub)}</text>'
            )
        else:
            self.parts.append(
                f'<text x="{cx:.1f}" y="{y + h / 2 + 4.5:.1f}" text-anchor="middle" font-family="system-ui,-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif" '
                f'font-size="{ts}" font-weight="600" fill="{c["text"]}">{esc(title)}</text>'
            )

    def label(self, x, y, s, size=10.5, anchor="start", tone="muted", bold=True, ls=0.7):
        c = self.c
        self.parts.append(
            f'<text x="{x:.1f}" y="{y:.1f}" text-anchor="{anchor}" font-family="system-ui,-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif" '
            f'font-size="{size}" font-weight="{"600" if bold else "400"}" letter-spacing="{ls}" fill="{c[tone]}">{esc(s)}</text>'
        )

    def path(self, d, arrow=True, back=False, dash=None):
        c = self.c
        m = ''
        if arrow:
            m += ' marker-end="url(#a)"'
        if back:
            m += ' marker-start="url(#b)"'
        da = f' stroke-dasharray="{dash}"' if dash else ''
        self.parts.append(
            f'<path d="{d}" fill="none" stroke="{c["line"]}" stroke-width="1.4"{da}{m}/>'
        )

    def vline(self, x, y1, y2, **kw):
        self.path(f"M{x:.1f} {y1:.1f} L{x:.1f} {y2:.1f}", **kw)

    def hline(self, x1, x2, y, **kw):
        self.path(f"M{x1:.1f} {y:.1f} L{x2:.1f} {y:.1f}", **kw)

    def render(self, title, desc):
        c = self.c
        return (
            f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {self.w} {self.h}" '
            f'width="{self.w}" height="{self.h}" role="img" aria-labelledby="t d">'
            f'<title id="t">{esc(title)}</title><desc id="d">{esc(desc)}</desc>'
            + self.defs()
            + f'<rect width="{self.w}" height="{self.h}" fill="none"/>'
            + "".join(self.parts)
            + "</svg>\n"
        )


def write(name, builder, title, desc):
    for suffix, pal in (("", LIGHT), ("-dark", DARK)):
        s = builder(pal)
        path = os.path.join(OUT, f"{name}{suffix}.svg")
        with open(path, "w") as f:
            f.write(s.render(title, desc))
        print("wrote", path)

# ---------------------------------------------------------------- D1
def d1(c):
    s = Svg(660, 412, c)
    X, W = 110, 420
    s.box(X, 18, W, 58, "Context Pipeline",
          "assemble task, enterprise, runtime, and memory state", "b2")
    s.vline(320, 76, 102)
    s.box(X, 102, W, 42, "Model decision", None, "b1")
    s.vline(320, 144, 170)
    s.box(X, 170, W, 58, "Policy + provider admission",
          "bind identity, capability, permission, and execution route", "b3")
    s.vline(320, 228, 254)
    s.box(X, 254, W, 58, "Runner inside the owning environment",
          "tools · workspace · private network · enterprise systems", "b4")
    s.vline(320, 312, 338)
    s.box(X, 338, W, 42, "Trace · Introspect · Explain · Reflect", None, "b5")
    s.path("M530 359 L600 359 L600 47 L530 47")
    s.parts.append(
        f'<text x="622" y="203" text-anchor="middle" transform="rotate(-90 622 203)" '
        f'font-family="system-ui,-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif" '
        f'font-size="11" fill="{c["muted"]}">durable Work and future context</text>'
    )
    return s

# ---------------------------------------------------------------- D2
def d2(c):
    s = Svg(900, 640, c)
    X, W, CX = 140, 680, 480
    s.label(X, 26, "EXPERIENCE")
    s.box(X, 34, W, 46, "Web dashboard · CLI/TUI · TypeScript SDK · API clients", None, "b1")
    s.vline(CX, 80, 106)
    s.label(X, 118, "CONTROL")
    s.box(X, 126, W, 46,
          "Server · Session/Run/Work · identity · orchestration · checkpoints", None, "b2")
    s.vline(CX, 172, 198)
    s.label(X, 200, "INTELLIGENCE")
    s.box(140, 208, 190, 48, "Context Pipeline", None, "b2")
    s.hline(330, 370, 232)
    s.box(370, 208, 170, 48, "Model decision", None, "b1")
    s.hline(540, 580, 232)
    s.box(580, 208, 240, 48, "Policy + provider decision", None, "b3")
    s.vline(700, 256, 290, arrow=False)
    s.hline(250, 730, 290, arrow=False)
    s.label(X, 308, "EXECUTION")
    for bx, bw, t, sub, tone, cx in (
        (140, 220, "Server provider", "shared state · control plane", "b1", 250),
        (400, 180, "User Runner", "CLI or Edge", "b4", 490),
        (640, 180, "MCP / sandbox", "scoped runtime", "b3", 730),
    ):
        s.vline(cx, 290, 316)
        s.box(bx, 316, bw, 56, t, sub, tone)
    s.vline(490, 372, 402)
    s.box(340, 402, 300, 56, "Private enterprise IT",
          "workspace · network · tools · data", "b4")
    s.vline(490, 458, 488)
    s.label(X, 480, "EVIDENCE")
    s.box(X, 488, W, 46, "Trace · Introspect · Explain · Reflect · Audit", None, "b5")
    s.path("M140 511 L100 511 L100 232 L140 232")
    s.parts.append(
        f'<text x="88" y="350" text-anchor="middle" transform="rotate(-90 88 350)" '
        f'font-family="system-ui,-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif" '
        f'font-size="11" fill="{c["muted"]}">future context</text>'
    )
    s.vline(CX, 534, 566, dash="4 4")
    s.label(X, 560, "DURABLE FACTS")
    s.box(X, 566, W, 46,
          "MatrixOne · Memoria · transcript · artifacts · checkpoints · trace · audit",
          None, "b5")
    return s

# ---------------------------------------------------------------- D3
def d3(c):
    s = Svg(880, 320, c)
    inputs = [
        "System contract", "Session · Run · Work", "Memory · enterprise facts",
        "Artifacts · tool results", "Runner · provider · policy", "Trace · reflection",
    ]
    y = 56
    for t in inputs:
        s.label(250, y, t, size=12, anchor="end", tone="text", bold=False, ls=0)
        s.hline(258, 278, y - 4, arrow=False)
        y += 28
    s.vline(278, 48, 194, arrow=False)
    s.hline(278, 310, 121)
    s.box(310, 99, 130, 44, "assemble", None, "b2")
    s.hline(440, 480, 121)
    s.box(480, 99, 210, 44, "select · budget · compress", None, "b2")
    s.hline(690, 730, 121)
    s.box(730, 99, 110, 44, "model", None, "b1")
    s.path("M785 143 L785 228 L615 228 L615 250")
    s.box(470, 250, 290, 44, "decision · usage · checkpoint", None, "b5")
    s.path("M470 272 L160 272 L160 222")
    s.label(170, 252, "future context", size=11, tone="muted", bold=False, ls=0)
    return s

# ---------------------------------------------------------------- D4
def d4(c):
    s = Svg(860, 320, c)
    s.box(30, 160, 110, 42, "queued", None, "b5")
    s.hline(140, 180, 181)
    s.box(180, 160, 120, 42, "running", None, "b1")
    s.hline(300, 680, 181)
    s.box(680, 160, 140, 42, "completed", None, "b4")
    s.vline(350, 43, 279, arrow=False)
    s.parts.append(f'<circle cx="350" cy="181" r="3.2" fill="{c["line"]}"/>')
    for yy, t, tone, both in (
        (43, "waiting", "b3", True),
        (89, "paused", "b3", True),
        (135, "blocked", "b3", True),
        (233, "cancelling", "b6", False),
        (279, "failed", "b6", False),
    ):
        s.hline(350, 400, yy, back=both)
        s.box(400, yy - 19, 150, 38, t, None, tone)
    s.hline(550, 610, 233)
    s.box(610, 214, 140, 38, "cancelled", None, "b6")
    return s

# ---------------------------------------------------------------- D5
def d5(c):
    s = Svg(700, 210, c)
    s.box(20, 40, 140, 44, "Runtime facts", None, "b5")
    s.hline(160, 200, 62)
    s.box(200, 40, 110, 44, "Trace", None, "b1")
    s.hline(310, 350, 62)
    s.box(350, 40, 130, 44, "Introspect", None, "b2")
    s.path("M480 55 C505 55, 505 34, 530 34")
    s.box(530, 14, 130, 40, "Explain", None, "b4")
    s.path("M480 70 C505 70, 505 90, 530 90")
    s.box(530, 70, 130, 40, "Reflect", None, "b3")
    s.box(20, 150, 160, 44, "Policy decisions", None, "b3")
    s.path("M255 84 L255 172 L350 172")
    s.hline(180, 350, 172)
    s.box(350, 150, 130, 44, "Audit", None, "b5")
    return s


write("context-to-execution", d1, "From context to execution",
      "Vertical flow: the Context Pipeline assembles state, the model decides, policy and provider "
      "admission binds identity and route, a Runner executes inside the owning environment, and "
      "Trace, Introspect, Explain and Reflect feed durable Work and future context back into the pipeline.")
write("architecture", d2, "Astra architecture",
      "Experience surfaces (Web dashboard, CLI/TUI, TypeScript SDK, API clients) sit above a durable "
      "control backbone. The Context Pipeline, model decision and policy decision fan out to execution "
      "capacity: Server provider, User Runner on CLI or Edge, and MCP or sandbox. The User Runner "
      "reaches private enterprise IT. Evidence (Trace, Introspect, Explain, Reflect, Audit) returns to "
      "the Context Pipeline as future context, over durable facts in MatrixOne and Memoria.")
write("context-pipeline", d3, "Context Pipeline",
      "Six structured inputs (system contract, Session/Run/Work, memory and enterprise facts, "
      "artifacts and tool results, Runner/provider/policy, trace and reflection) are assembled, then "
      "selected, budgeted and compressed into the model boundary. Decision, usage and checkpoint "
      "output returns as future context.")
write("run-lifecycle", d4, "Run lifecycle states",
      "A run moves from queued to running to completed. From running it can enter waiting, paused or "
      "blocked and return to running, or move to cancelling and then cancelled, or to failed.")
write("observation-plane", d5, "Observation plane",
      "Runtime facts flow into Trace, then Introspect, which projects into Explain and Reflect. "
      "Trace and policy decisions both feed Audit.")

def d6(c):
    s = Svg(660, 380, c)
    X, W, CX = 110, 420, 320
    def elabel(y, t):
        s.parts.append(
            f'<text x="{CX + 12}" y="{y}" font-family="system-ui,-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif" '
            f'font-size="11" fill="{c["muted"]}">{esc(t)}</text>')
    s.box(X, 18, W, 40, "User / app", None, "b5")
    s.vline(CX, 58, 100); elabel(83, "submit durable Work")
    s.box(X, 100, W, 58, "Astra Server",
          "durable Work · identity · context · policy · provider decision", "b2")
    s.vline(CX, 158, 200); elabel(183, "admitted tool call")
    s.box(X, 200, W, 58, "User Runner",
          "inside the user or enterprise trust boundary", "b4")
    s.vline(CX, 258, 292)
    s.box(X, 292, W, 58, "Private enterprise IT",
          "file · shell · Git · builds · private network · local MCP", "b4")
    s.path("M530 321 L600 321 L600 129 L530 129")
    s.parts.append(
        f'<text x="622" y="225" text-anchor="middle" transform="rotate(-90 622 225)" '
        f'font-family="system-ui,-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif" '
        f'font-size="11" fill="{c["muted"]}">typed result + execution identity + evidence</text>')
    return s

write("runner-boundary", d6, "Runner execution boundary",
      "A user or app submits durable Work to the Astra Server, which owns durable Work, identity, "
      "context, policy and the provider decision. An admitted tool call goes to a User Runner inside "
      "the user or enterprise trust boundary, which reaches private enterprise IT: file, shell, Git, "
      "builds, private network and local MCP. A typed result with execution identity and evidence "
      "returns to the backbone.")
