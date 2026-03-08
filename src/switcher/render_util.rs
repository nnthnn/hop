/// XRender drawing utilities: pixel scaling, rounded rects, border rings, color conversion.

use std::error::Error;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _, Arc, CreateGCAux, ChangeGCAux, Rectangle, Visualid};
use x11rb::protocol::render::{
    ConnectionExt as RenderConnectionExt,
    Color as RenderColor,
    PictOp, PictType, CreatePictureAux, Repeat,
};
use x11rb::rust_connection::RustConnection;

use super::PictCtx;

/// Downscale `pixels` (packed ARGB u32, `src_w × src_h`) to `dst_w × dst_h`
/// by averaging each output pixel's contributing source region.
pub(super) fn downscale_argb(pixels: &[u32], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u32> {
    let mut out = vec![0u32; (dst_w * dst_h) as usize];
    let x_ratio = src_w as f64 / dst_w as f64;
    let y_ratio = src_h as f64 / dst_h as f64;
    for dy in 0..dst_h {
        let y0 = (dy as f64 * y_ratio) as u32;
        let y1 = ((dy + 1) as f64 * y_ratio) as u32;
        let y1 = y1.min(src_h);
        for dx in 0..dst_w {
            let x0 = (dx as f64 * x_ratio) as u32;
            let x1 = ((dx + 1) as f64 * x_ratio) as u32;
            let x1 = x1.min(src_w);
            let (mut r, mut g, mut b, mut a, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = pixels[(sy * src_w + sx) as usize];
                    a += ((p >> 24) & 0xFF) as u64;
                    r += ((p >> 16) & 0xFF) as u64;
                    g += ((p >>  8) & 0xFF) as u64;
                    b += ( p        & 0xFF) as u64;
                    n += 1;
                }
            }
            if n > 0 {
                out[(dy * dst_w + dx) as usize] =
                    (((a/n) as u32) << 24) | (((r/n) as u32) << 16) | (((g/n) as u32) << 8) | (b/n) as u32;
            }
        }
    }
    out
}

/// Walk up the window hierarchy to find the WM frame: the direct child of root
/// that contains `client`. Compositors redirect only direct children of root,
/// so NameWindowPixmap must be called on the frame, not the client window.
pub(super) fn find_frame_win(conn: &RustConnection, client: xproto::Window, root: xproto::Window) -> xproto::Window {
    let mut w = client;
    loop {
        match conn.query_tree(w).ok().and_then(|c| c.reply().ok()) {
            Some(t) if t.parent == root || t.parent == 0 => return w,
            Some(t) if t.parent != w => w = t.parent,
            _ => return client,
        }
    }
}

/// Find the XRender PictFormat ID for the given visual on the given screen.
/// Used to create a Picture from a composite backing pixmap whose depth may differ
/// from our ARGB32 popup (e.g., a 24-bit window has a 24-bit format).
pub(super) fn find_format_for_visual(
    formats: &x11rb::protocol::render::QueryPictFormatsReply,
    visual_id: Visualid,
    screen_num: usize,
) -> Option<u32> {
    let screen = formats.screens.get(screen_num)?;
    for depth in &screen.depths {
        for visual in &depth.visuals {
            if visual.visual == visual_id {
                return Some(visual.format);
            }
        }
    }
    None
}

/// Find the XRender A8 (8-bit alpha-only) picture format.
pub(super) fn find_a8_format(formats: &x11rb::protocol::render::QueryPictFormatsReply) -> Option<u32> {
    formats.formats.iter()
        .find(|f| {
            f.depth == 8
                && f.type_ == PictType::DIRECT
                && f.direct.alpha_mask == 0xFF
                && f.direct.red_mask == 0
                && f.direct.green_mask == 0
                && f.direct.blue_mask == 0
        })
        .map(|f| f.id)
}

/// Draw a filled rounded rectangle shape into a pixmap using a GC.
/// The GC's current foreground pixel value is used for the fill.
/// Painting order: three rectangles for the interior cross, four filled arcs for corners.
pub(super) fn fill_rounded_rect_to_gc(
    conn: &RustConnection,
    pix: u32,
    gc: u32,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    radius: u32,
) -> Result<(), Box<dyn Error>> {
    let r = (radius as u16).min(w / 2).min(h / 2);
    if r == 0 {
        conn.poly_fill_rectangle(pix, gc, &[Rectangle { x, y, width: w, height: h }])?.check()?;
        return Ok(());
    }
    let r2 = r * 2;
    conn.poly_fill_rectangle(pix, gc, &[
        Rectangle { x: x + r as i16,           y,               width: w - r2, height: h      },
        Rectangle { x,                          y: y + r as i16, width: r,      height: h - r2 },
        Rectangle { x: x + (w - r) as i16,     y: y + r as i16, width: r,      height: h - r2 },
    ])?.check()?;
    conn.poly_fill_arc(pix, gc, &[
        Arc { x,                     y,                     width: r2, height: r2, angle1: 90*64,  angle2: 90*64 }, // top-left
        Arc { x: x+(w-r2) as i16,   y,                     width: r2, height: r2, angle1: 0,      angle2: 90*64 }, // top-right
        Arc { x: x+(w-r2) as i16,   y: y+(h-r2) as i16,   width: r2, height: r2, angle1: 17280,  angle2: 90*64 }, // bottom-right (270*64)
        Arc { x,                     y: y+(h-r2) as i16,   width: r2, height: r2, angle1: 180*64, angle2: 90*64 }, // bottom-left
    ])?.check()?;
    Ok(())
}

/// Composite a solid color through an A8 rounded-rect mask onto `dst_pic` (OVER).
///
/// When `radius == 0` falls back to a plain `render_fill_rectangles` call.
pub(super) fn draw_filled_rounded_rect(
    conn: &RustConnection,
    ctx: PictCtx,
    x: i16,
    y: i16,
    w: u32,
    h: u32,
    radius: u32,
    color_argb: u32,
) -> Result<(), Box<dyn Error>> {
    if w == 0 || h == 0 { return Ok(()); }
    let wu = w as u16;
    let hu = h as u16;

    if radius == 0 {
        let (cr, cg, cb, ca) = argb_to_render_color(color_argb);
        conn.render_fill_rectangles(
            PictOp::OVER, ctx.pic,
            RenderColor { red: cr, green: cg, blue: cb, alpha: ca },
            &[Rectangle { x, y, width: wu, height: hu }],
        )?.check()?;
        return Ok(());
    }

    let mask_pix = conn.generate_id()?;
    conn.create_pixmap(8, mask_pix, ctx.drawable, wu, hu)?.check()?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, mask_pix, &CreateGCAux::new().foreground(0u32))?.check()?;
    conn.poly_fill_rectangle(mask_pix, gc, &[Rectangle { x: 0, y: 0, width: wu, height: hu }])?.check()?;
    conn.change_gc(gc, &ChangeGCAux::new().foreground(255u32))?.check()?;
    fill_rounded_rect_to_gc(conn, mask_pix, gc, 0, 0, wu, hu, radius)?;
    conn.free_gc(gc)?.check()?;

    composite_color_through_mask(conn, ctx, x, y, wu, hu, mask_pix, color_argb)?;
    conn.free_pixmap(mask_pix)?.check()?;
    Ok(())
}

/// Composite a solid color through a ring-shaped A8 mask onto `dst_pic` (OVER).
///
/// The ring = outer rounded rect minus the inner rounded rect punched out.
/// `(ox, oy, ow, oh)` is the outer rect; the inner rect is inset by `fw` on all sides.
/// `outer_r` is the corner radius of the outer edge; `inner_r` for the inner edge
/// (typically `outer_r - fw`, or 0 when `outer_r <= fw`).
pub(super) fn draw_border_ring(
    conn: &RustConnection,
    ctx: PictCtx,
    ox: i16,
    oy: i16,
    ow: u32,
    oh: u32,
    outer_r: u32,
    fw: u32,
    inner_r: u32,
    color_argb: u32,
) -> Result<(), Box<dyn Error>> {
    if ow == 0 || oh == 0 { return Ok(()); }
    let owu = ow as u16;
    let ohu = oh as u16;

    let mask_pix = conn.generate_id()?;
    conn.create_pixmap(8, mask_pix, ctx.drawable, owu, ohu)?.check()?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, mask_pix, &CreateGCAux::new().foreground(0u32))?.check()?;
    // Clear mask to 0.
    conn.poly_fill_rectangle(mask_pix, gc, &[Rectangle { x: 0, y: 0, width: owu, height: ohu }])?.check()?;
    // Paint outer rounded rect with 255.
    conn.change_gc(gc, &ChangeGCAux::new().foreground(255u32))?.check()?;
    fill_rounded_rect_to_gc(conn, mask_pix, gc, 0, 0, owu, ohu, outer_r)?;
    // Punch out inner rounded rect with 0.
    let iw = ow.saturating_sub(2 * fw) as u16;
    let ih = oh.saturating_sub(2 * fw) as u16;
    if iw > 0 && ih > 0 {
        conn.change_gc(gc, &ChangeGCAux::new().foreground(0u32))?.check()?;
        fill_rounded_rect_to_gc(conn, mask_pix, gc, fw as i16, fw as i16, iw, ih, inner_r)?;
    }
    conn.free_gc(gc)?.check()?;

    composite_color_through_mask(conn, ctx, ox, oy, owu, ohu, mask_pix, color_argb)?;
    conn.free_pixmap(mask_pix)?.check()?;
    Ok(())
}

/// Composite `color_argb` through an existing A8 `mask_pix` onto `ctx.pic` using OVER.
/// Creates a temporary 1×1 repeated source and frees it afterwards.
/// Does NOT free `mask_pix` — the caller is responsible for that.
pub(super) fn composite_color_through_mask(
    conn: &RustConnection,
    ctx: PictCtx,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    mask_pix: u32,
    color_argb: u32,
) -> Result<(), Box<dyn Error>> {
    let mask_pic = conn.generate_id()?;
    conn.render_create_picture(mask_pic, mask_pix, ctx.a8_fmt, &CreatePictureAux::new())?.check()?;

    let src_pix = conn.generate_id()?;
    conn.create_pixmap(32, src_pix, ctx.drawable, 1, 1)?.check()?;
    let src_pic = conn.generate_id()?;
    conn.render_create_picture(src_pic, src_pix, ctx.argb_fmt,
        &CreatePictureAux::new().repeat(Repeat::NORMAL))?.check()?;
    conn.free_pixmap(src_pix)?.check()?;

    let (cr, cg, cb, ca) = argb_to_render_color(color_argb);
    conn.render_fill_rectangles(
        PictOp::SRC, src_pic,
        RenderColor { red: cr, green: cg, blue: cb, alpha: ca },
        &[Rectangle { x: 0, y: 0, width: 1, height: 1 }],
    )?.check()?;

    conn.render_composite(
        PictOp::OVER,
        src_pic, mask_pic, ctx.pic,
        0, 0,  // src x, y  (1×1 with repeat)
        0, 0,  // mask x, y
        x, y,  // dst x, y
        w, h,
    )?.check()?;

    conn.render_free_picture(src_pic)?.check()?;
    conn.render_free_picture(mask_pic)?.check()?;
    Ok(())
}

/// Convert a packed 0xAARRGGBB u32 into XRender color components (0–0xFFFF each).
pub(super) fn argb_to_render_color(argb: u32) -> (u16, u16, u16, u16) {
    let a = ((argb >> 24) & 0xFF) as u16;
    let r = ((argb >> 16) & 0xFF) as u16;
    let g = ((argb >>  8) & 0xFF) as u16;
    let b = ( argb        & 0xFF) as u16;
    // Scale 0–255 → 0–65535 with premultiplied alpha for XRender
    let scale = |c: u16| c * 0x101;
    let alpha = scale(a);
    // XRender expects premultiplied color components
    let premul = |c: u16| (c as u32 * a as u32 / 255) as u16 * 0x101;
    (premul(r), premul(g), premul(b), alpha)
}
