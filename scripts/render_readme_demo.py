#!/usr/bin/env python3
"""Render the short illustrative context-to-execution flow in README.md."""

from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont


ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "docs/assets/astra-cli-demo.gif"
FONT_DIR = ROOT / "web/public/fonts/inconsolata/files"

WIDTH = 1280
HEIGHT = 720
FPS = 5
DURATION_SECONDS = 20

BACKGROUND = "#090d12"
PANEL = "#11171f"
PANEL_TOP = "#171e27"
SURFACE = "#151d27"
BORDER = "#293442"
TEXT = "#e7eef8"
MUTED = "#6f8196"
ACCENT = "#6ca9ff"
GUTTER = "#cd84ff"
SUCCESS = "#36e29d"
WARN = "#f4c066"
ERROR = "#ff6770"


def font(size: int, weight: int = 400) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(str(FONT_DIR / f"inconsolata-{weight}.ttf"), size)


SMALL = font(18)
BODY = font(21)
BODY_BOLD = font(21, 600)
LABEL = font(16, 600)
TITLE = font(30, 700)


def fade(elapsed: float, start: float, duration: float = 0.6) -> float:
    return max(0.0, min(1.0, (elapsed - start) / duration))


def blend(color: str, amount: float, base: str = PANEL) -> tuple[int, int, int]:
    foreground = tuple(int(color[index : index + 2], 16) for index in (1, 3, 5))
    background = tuple(int(base[index : index + 2], 16) for index in (1, 3, 5))
    return tuple(
        round(background[channel] + (foreground[channel] - background[channel]) * amount)
        for channel in range(3)
    )


def card(
    draw: ImageDraw.ImageDraw,
    bounds: tuple[int, int, int, int],
    intensity: float = 1.0,
    outline: str = BORDER,
) -> None:
    draw.rounded_rectangle(
        bounds,
        radius=14,
        fill=blend(SURFACE, intensity),
        outline=blend(outline, intensity),
        width=2,
    )


def dashed_boundary(
    draw: ImageDraw.ImageDraw,
    bounds: tuple[int, int, int, int],
    color: tuple[int, int, int],
) -> None:
    left, top, right, bottom = bounds
    for x in range(left, right, 16):
        draw.line((x, top, min(x + 8, right), top), fill=color, width=2)
        draw.line((x, bottom, min(x + 8, right), bottom), fill=color, width=2)
    for y in range(top, bottom, 16):
        draw.line((left, y, left, min(y + 8, bottom)), fill=color, width=2)
        draw.line((right, y, right, min(y + 8, bottom)), fill=color, width=2)


def arrow(
    draw: ImageDraw.ImageDraw,
    start: tuple[int, int],
    end: tuple[int, int],
    progress: float,
    color: str,
) -> None:
    draw.line((*start, *end), fill=BORDER, width=2)
    if progress <= 0:
        return
    x = round(start[0] + (end[0] - start[0]) * progress)
    y = round(start[1] + (end[1] - start[1]) * progress)
    draw.line((*start, x, y), fill=color, width=3)
    draw.ellipse((x - 5, y - 5, x + 5, y + 5), fill=color)
    if progress >= 0.98:
        direction = 1 if end[0] >= start[0] else -1
        draw.polygon(
            [(end[0], end[1]), (end[0] - direction * 12, end[1] - 7), (end[0] - direction * 12, end[1] + 7)],
            fill=color,
        )


def stage_row(
    draw: ImageDraw.ImageDraw,
    y: int,
    name: str,
    detail: str,
    color: str,
    intensity: float,
) -> None:
    draw.ellipse((392, y + 5, 402, y + 15), fill=blend(color, intensity, SURFACE))
    draw.text((416, y), name, font=LABEL, fill=blend(color, intensity, SURFACE))
    draw.text((510, y - 2), detail, font=SMALL, fill=blend(TEXT, 0.35 + 0.65 * intensity, SURFACE))


def outcome_item(
    draw: ImageDraw.ImageDraw,
    x: int,
    label: str,
    color: str,
    intensity: float,
) -> int:
    draw.text((x, 635), "✓", font=BODY_BOLD, fill=blend(color, intensity, PANEL_TOP))
    draw.text((x + 25, 636), label, font=LABEL, fill=blend(TEXT, 0.35 + 0.65 * intensity, PANEL_TOP))
    return x + 25 + round(draw.textlength(label, font=LABEL)) + 62


def draw_frame(frame_number: int) -> Image.Image:
    elapsed = frame_number / FPS
    image = Image.new("RGB", (WIDTH, HEIGHT), BACKGROUND)
    draw = ImageDraw.Draw(image)

    # Window chrome and explanatory title.
    draw.rounded_rectangle((30, 22, 1250, 700), radius=18, fill=PANEL, outline=BORDER, width=2)
    draw.rounded_rectangle((31, 23, 1249, 82), radius=17, fill=PANEL_TOP)
    draw.rectangle((31, 62, 1249, 82), fill=PANEL_TOP)
    for index, color in enumerate((ERROR, WARN, SUCCESS)):
        x = 58 + index * 27
        draw.ellipse((x, 45, x + 11, 56), fill=color)
    draw.text((548, 38), "ASTRA · DURABLE WORKFLOW", font=SMALL, fill=MUTED)
    draw.rounded_rectangle((1045, 39, 1217, 64), radius=12, outline=BORDER, width=1)
    draw.text((1064, 42), "ILLUSTRATIVE FLOW", font=LABEL, fill=MUTED)

    draw.text((64, 100), "Less context. Reversible changes. The same Work in every environment.", font=BODY, fill=TEXT)

    # Request surface.
    request_intensity = fade(elapsed, 0.4)
    card(draw, (64, 186, 316, 494), request_intensity)
    draw.text((86, 208), "CLI · WEB · SDK", font=LABEL, fill=blend(ACCENT, request_intensity, SURFACE))
    draw.text((86, 253), "Update release", font=TITLE, fill=blend(TEXT, request_intensity, SURFACE))
    draw.text((86, 286), "configuration", font=TITLE, fill=blend(TEXT, request_intensity, SURFACE))
    draw.line((86, 335, 294, 335), fill=blend(BORDER, request_intensity, SURFACE), width=1)
    draw.text((86, 356), "submitted once", font=SMALL, fill=blend(MUTED, request_intensity, SURFACE))
    draw.text((86, 385), "client may disconnect", font=SMALL, fill=blend(MUTED, request_intensity, SURFACE))
    request_status = "Work continues" if elapsed >= 17 else "request accepted"
    request_status_color = SUCCESS if elapsed >= 17 else ACCENT
    draw.text((86, 442), "●", font=SMALL, fill=blend(request_status_color, request_intensity, SURFACE))
    draw.text((111, 442), request_status, font=SMALL, fill=blend(request_status_color, request_intensity, SURFACE))

    # Durable server backbone.
    server_intensity = fade(elapsed, 2.4)
    card(draw, (356, 146, 822, 566), server_intensity, ACCENT)
    draw.text((382, 168), "ASTRA SERVER", font=LABEL, fill=blend(ACCENT, server_intensity, SURFACE))
    draw.text((382, 195), "durable control backbone", font=SMALL, fill=blend(MUTED, server_intensity, SURFACE))

    work_intensity = fade(elapsed, 2.8)
    draw.rounded_rectangle((382, 236, 796, 304), radius=10, fill=blend("#1b2734", work_intensity, SURFACE))
    draw.text((400, 250), "Work w_18a4", font=BODY_BOLD, fill=blend(TEXT, work_intensity, SURFACE))
    work_status = "completed" if elapsed >= 16.8 else "running · durable"
    work_color = SUCCESS if elapsed >= 16.8 else GUTTER
    draw.text((632, 251), work_status, font=SMALL, fill=blend(work_color, work_intensity, SURFACE))
    draw.text((400, 279), "survives requests · moves across environments", font=LABEL, fill=blend(MUTED, work_intensity, SURFACE))

    stage_row(draw, 334, "CONTEXT", "31% fewer tokens", ACCENT, fade(elapsed, 4.4))
    stage_row(draw, 378, "POLICY", "write_repo → user runner", WARN, fade(elapsed, 6.6))
    stage_row(draw, 422, "TRACE", "change · receipt · rollback", GUTTER, fade(elapsed, 14.6))

    result_intensity = fade(elapsed, 16.0)
    draw.rounded_rectangle(
        (382, 474, 796, 540),
        radius=10,
        fill=blend("#172b25", result_intensity, SURFACE),
        outline=blend(SUCCESS, result_intensity, SURFACE),
        width=1,
    )
    result_title = "↶ change rolled back" if elapsed >= 17.0 else "✓ 2 files changed · rollback ready"
    result_detail = "Work continues · evidence retained" if elapsed >= 17.0 else "pre-images captured · receipt r_72c1"
    draw.text((400, 489), result_title, font=BODY_BOLD, fill=blend(SUCCESS, result_intensity, SURFACE))
    draw.text((400, 515), result_detail, font=LABEL, fill=blend(MUTED, result_intensity, SURFACE))

    # Private execution environment and its local authority.
    boundary_intensity = fade(elapsed, 7.8)
    dashed_boundary(draw, (862, 124, 1216, 566), blend(SUCCESS, boundary_intensity))
    draw.text((884, 139), "USER ENVIRONMENT", font=LABEL, fill=blend(SUCCESS, boundary_intensity, PANEL))
    draw.text((884, 164), "private access stays here", font=SMALL, fill=blend(MUTED, boundary_intensity, PANEL))

    card(draw, (884, 210, 1194, 350), boundary_intensity, SUCCESS)
    draw.text((906, 231), "USER RUNNER", font=BODY_BOLD, fill=blend(SUCCESS, boundary_intensity, SURFACE))
    draw.text((906, 268), "identity", font=LABEL, fill=blend(MUTED, boundary_intensity, SURFACE))
    draw.text((1000, 266), "alice", font=SMALL, fill=blend(TEXT, boundary_intensity, SURFACE))
    draw.text((906, 299), "workspace", font=LABEL, fill=blend(MUTED, boundary_intensity, SURFACE))
    draw.text((1000, 297), "payments", font=SMALL, fill=blend(TEXT, boundary_intensity, SURFACE))
    draw.text((906, 330), "network", font=LABEL, fill=blend(MUTED, boundary_intensity, SURFACE))
    draw.text((1000, 328), "private", font=SMALL, fill=blend(TEXT, boundary_intensity, SURFACE))

    resources_intensity = fade(elapsed, 10.4)
    resource_x = 884
    for label in ("repo", "database", "internal API"):
        width = round(draw.textlength(label, font=LABEL)) + 30
        draw.rounded_rectangle(
            (resource_x, 390, resource_x + width, 423),
            radius=16,
            fill=blend("#172b25", resources_intensity, PANEL),
            outline=blend(SUCCESS, resources_intensity, PANEL),
            width=1,
        )
        draw.text((resource_x + 15, 399), label, font=LABEL, fill=blend(TEXT, resources_intensity, PANEL))
        resource_x += width + 12

    action_intensity = fade(elapsed, 11.2)
    action_label = "rollback_file_edits" if elapsed >= 17.0 else "edit_release_config"
    draw.text((884, 460), "●", font=BODY, fill=blend(SUCCESS, action_intensity, PANEL))
    draw.text((912, 460), action_label, font=BODY, fill=blend(TEXT, action_intensity, PANEL))
    draw.text((884, 494), "captured change + execution identity", font=SMALL, fill=blend(MUTED, action_intensity, PANEL))

    # Animated request, admitted tool call, and typed result.
    arrow(draw, (316, 274), (356, 274), fade(elapsed, 1.7, 0.8), ACCENT)
    draw.text((319, 244), "Work", font=LABEL, fill=blend(ACCENT, fade(elapsed, 1.7), PANEL))
    arrow(draw, (822, 330), (862, 330), fade(elapsed, 8.6, 1.1), WARN)
    draw.text((824, 300), "admitted", font=LABEL, fill=blend(WARN, fade(elapsed, 8.6), PANEL))
    arrow(draw, (862, 372), (822, 372), fade(elapsed, 12.7, 1.1), SUCCESS)
    draw.text((831, 382), "evidence", font=LABEL, fill=blend(SUCCESS, fade(elapsed, 12.7), PANEL))

    # Outcome: the three product promises illustrated by the flow.
    draw.rounded_rectangle((64, 608, 1216, 680), radius=12, fill=PANEL_TOP, outline=BORDER, width=1)
    item_x = 82
    item_x = outcome_item(draw, item_x, "DURABLE WORK ON FEWER TOKENS", GUTTER, fade(elapsed, 4.4))
    item_x = outcome_item(draw, item_x, "AGENT CHANGES YOU CAN TRACE AND ROLL BACK", SUCCESS, fade(elapsed, 14.6))
    outcome_item(draw, item_x, "WORK THAT MOVES WITH YOU", ACCENT, fade(elapsed, 8.6))

    return image


def main() -> None:
    frames = [draw_frame(frame) for frame in range(FPS * DURATION_SECONDS)]
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    frames[0].save(
        OUTPUT,
        save_all=True,
        append_images=frames[1:],
        duration=round(1000 / FPS),
        loop=0,
        optimize=True,
        disposal=2,
    )
    print(f"rendered {OUTPUT.relative_to(ROOT)} ({DURATION_SECONDS}s at {FPS} fps)")


if __name__ == "__main__":
    main()
