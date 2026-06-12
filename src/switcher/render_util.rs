//! XRender drawing utilities: pixel scaling, rounded rects, border rings, color conversion.

use std::error::Error;
use x11rb::protocol::xproto::{self, ConnectionExt as _, Arc, CreateGCAux, ChangeGCAux, Rectangle};
use x11rb::protocol::render::{
    ConnectionExt as RenderConnectionExt,
    Color as RenderColor,
    PictOp, PictType, CreatePictureAux, Repeat,
};
use x11rb::rust_connection::RustConnection;

use super::{PictCtx, Rect};
use super::resource::{PixmapGuard, GcGuard, PictureGuard};

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
            // Empty source region — leave the output pixel at its initialized 0.
            if n == 0 { continue; }
            out[(dy * dst_w + dx) as usize] =
                (((a/n) as u32) << 24) | (((r/n) as u32) << 16) | (((g/n) as u32) << 8) | (b/n) as u32;
        }
    }
    out
}

/// Resolve the WM frame for each client window: the direct child of root that
/// contains it. Compositors redirect only direct children of root, so
/// NameWindowPixmap must target the frame, not the client window.
///
/// Each level of the hierarchy walk issues one `query_tree` per still-unresolved
/// client and collects the replies together, so N clients cost ~depth round-trips
/// instead of N×depth. Clients that fail to resolve fall back to themselves.
pub(super) fn find_frames_batched(
    conn: &RustConnection,
    clients: &[xproto::Window],
    root: xproto::Window,
) -> Vec<xproto::Window> {
    let n = clients.len();
    let mut result: Vec<xproto::Window> = clients.to_vec(); // fallback = client itself
    let mut done: Vec<bool> = vec![false; n];
    let mut cur: Vec<xproto::Window> = clients.to_vec();     // window under inspection

    // Bound the walk defensively against deep hierarchies or cycles.
    for _ in 0..32 {
        // Fire off query_tree for every still-active client, then collect replies.
        let cookies: Vec<_> = (0..n)
            .map(|i| if done[i] { None } else { conn.query_tree(cur[i]).ok() })
            .collect();
        let mut any_active = false;
        for (i, cookie) in cookies.into_iter().enumerate() {
            if done[i] { continue; }
            match cookie.and_then(|c| c.reply().ok()) {
                Some(t) if t.parent == root || t.parent == 0 => {
                    result[i] = cur[i];
                    done[i] = true;
                }
                Some(t) if t.parent != cur[i] => {
                    cur[i] = t.parent;
                    any_active = true;
                }
                _ => done[i] = true,
            }
        }
        if !any_active { break; }
    }
    result
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
    rect: Rect,
    radius: u32,
) -> Result<(), Box<dyn Error>> {
    let (x, y) = (rect.x, rect.y);
    let w = rect.w as u16;
    let h = rect.h as u16;
    let r = (radius as u16).min(w / 2).min(h / 2);
    if r == 0 {
        conn.poly_fill_rectangle(pix, gc, &[Rectangle { x, y, width: w, height: h }])?;
        return Ok(());
    }
    let r2 = r * 2;
    conn.poly_fill_rectangle(pix, gc, &[
        Rectangle { x: x + r as i16,           y,               width: w - r2, height: h      },
        Rectangle { x,                          y: y + r as i16, width: r,      height: h - r2 },
        Rectangle { x: x + (w - r) as i16,     y: y + r as i16, width: r,      height: h - r2 },
    ])?;
    conn.poly_fill_arc(pix, gc, &[
        Arc { x,                     y,                     width: r2, height: r2, angle1: 90*64,  angle2: 90*64 }, // top-left
        Arc { x: x+(w-r2) as i16,   y,                     width: r2, height: r2, angle1: 0,      angle2: 90*64 }, // top-right
        Arc { x: x+(w-r2) as i16,   y: y+(h-r2) as i16,   width: r2, height: r2, angle1: 17280,  angle2: 90*64 }, // bottom-right (270*64)
        Arc { x,                     y: y+(h-r2) as i16,   width: r2, height: r2, angle1: 180*64, angle2: 90*64 }, // bottom-left
    ])?;
    Ok(())
}

/// Composite a solid color through an A8 rounded-rect mask onto `dst_pic` (OVER).
///
/// When `radius == 0` falls back to a plain `render_fill_rectangles` call.
pub(super) fn draw_filled_rounded_rect(
    conn: &RustConnection,
    ctx: PictCtx,
    rect: Rect,
    radius: u32,
    color_argb: u32,
) -> Result<(), Box<dyn Error>> {
    if rect.w == 0 || rect.h == 0 { return Ok(()); }
    let (x, y) = (rect.x, rect.y);
    let wu = rect.w as u16;
    let hu = rect.h as u16;

    if radius == 0 {
        let (cr, cg, cb, ca) = argb_to_render_color(color_argb);
        conn.render_fill_rectangles(
            PictOp::OVER, ctx.pic,
            RenderColor { red: cr, green: cg, blue: cb, alpha: ca },
            &[Rectangle { x, y, width: wu, height: hu }],
        )?;
        return Ok(());
    }

    let mask_pix = PixmapGuard::create(conn, 8, ctx.drawable, wu, hu)?;
    let gc = GcGuard::create(conn, mask_pix.id, &CreateGCAux::new().foreground(0u32))?;
    conn.poly_fill_rectangle(mask_pix.id, gc.id, &[Rectangle { x: 0, y: 0, width: wu, height: hu }])?;
    conn.change_gc(gc.id, &ChangeGCAux::new().foreground(255u32))?;
    fill_rounded_rect_to_gc(conn, mask_pix.id, gc.id, Rect { x: 0, y: 0, w: rect.w, h: rect.h }, radius)?;
    drop(gc);

    composite_color_through_mask(conn, ctx, rect, mask_pix.id, color_argb)?;
    Ok(()) // mask_pix freed on drop
}

/// Composite a solid color through a ring-shaped A8 mask onto `dst_pic` (OVER).
///
/// The ring = outer rounded rect minus the inner rounded rect punched out.
/// `outer` is the outer rect; the inner rect is inset by `fw` on all sides.
/// `outer_r` is the corner radius of the outer edge; `inner_r` for the inner edge
/// (typically `outer_r - fw`, or 0 when `outer_r <= fw`).
pub(super) fn draw_border_ring(
    conn: &RustConnection,
    ctx: PictCtx,
    outer: Rect,
    outer_r: u32,
    fw: u32,
    inner_r: u32,
    color_argb: u32,
) -> Result<(), Box<dyn Error>> {
    if outer.w == 0 || outer.h == 0 { return Ok(()); }
    let owu = outer.w as u16;
    let ohu = outer.h as u16;

    let mask_pix = PixmapGuard::create(conn, 8, ctx.drawable, owu, ohu)?;
    let gc = GcGuard::create(conn, mask_pix.id, &CreateGCAux::new().foreground(0u32))?;
    // Clear mask to 0.
    conn.poly_fill_rectangle(mask_pix.id, gc.id, &[Rectangle { x: 0, y: 0, width: owu, height: ohu }])?;
    // Paint outer rounded rect with 255.
    conn.change_gc(gc.id, &ChangeGCAux::new().foreground(255u32))?;
    fill_rounded_rect_to_gc(conn, mask_pix.id, gc.id, Rect { x: 0, y: 0, w: outer.w, h: outer.h }, outer_r)?;
    // Punch out inner rounded rect with 0.
    let iw = outer.w.saturating_sub(2 * fw);
    let ih = outer.h.saturating_sub(2 * fw);
    if iw > 0 && ih > 0 {
        conn.change_gc(gc.id, &ChangeGCAux::new().foreground(0u32))?;
        fill_rounded_rect_to_gc(conn, mask_pix.id, gc.id, Rect { x: fw as i16, y: fw as i16, w: iw, h: ih }, inner_r)?;
    }
    drop(gc);

    composite_color_through_mask(conn, ctx, outer, mask_pix.id, color_argb)?;
    Ok(()) // mask_pix freed on drop
}

/// Composite `color_argb` through an existing A8 `mask_pix` onto `ctx.pic` using OVER.
/// Creates a temporary 1×1 repeated source and frees it afterwards.
/// Does NOT free `mask_pix` — the caller is responsible for that.
pub(super) fn composite_color_through_mask(
    conn: &RustConnection,
    ctx: PictCtx,
    rect: Rect,
    mask_pix: u32,
    color_argb: u32,
) -> Result<(), Box<dyn Error>> {
    // mask_pix is owned by the caller; we only wrap it in a picture here.
    let mask_pic = PictureGuard::create(conn, mask_pix, ctx.a8_fmt, &CreatePictureAux::new())?;

    let src_pix = PixmapGuard::create(conn, 32, ctx.drawable, 1, 1)?;
    let src_pic = PictureGuard::create(conn, src_pix.id, ctx.argb_fmt,
        &CreatePictureAux::new().repeat(Repeat::NORMAL))?;

    let (cr, cg, cb, ca) = argb_to_render_color(color_argb);
    conn.render_fill_rectangles(
        PictOp::SRC, src_pic.id,
        RenderColor { red: cr, green: cg, blue: cb, alpha: ca },
        &[Rectangle { x: 0, y: 0, width: 1, height: 1 }],
    )?;

    conn.render_composite(
        PictOp::OVER,
        src_pic.id, mask_pic.id, ctx.pic,
        0, 0,             // src x, y  (1×1 with repeat)
        0, 0,             // mask x, y
        rect.x, rect.y,   // dst x, y
        rect.w as u16, rect.h as u16,
    )?;
    Ok(()) // src_pix + pictures freed on drop
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downscale_uniform_color_is_preserved() {
        let src = vec![0xFF80_4020u32; 4]; // 2×2, all the same
        assert_eq!(downscale_argb(&src, 2, 2, 1, 1), vec![0xFF80_4020]);
    }

    #[test]
    fn downscale_averages_two_pixels() {
        // 2×1 black + white → 1×1 mid-grey, alpha preserved.
        let src = vec![0xFF00_0000u32, 0xFFFF_FFFF];
        assert_eq!(downscale_argb(&src, 2, 1, 1, 1), vec![0xFF7F_7F7F]);
    }

    #[test]
    fn argb_to_render_color_opaque_white() {
        // Opaque white → full-scale on every channel.
        assert_eq!(argb_to_render_color(0xFFFFFFFF), (0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF));
    }

    #[test]
    fn argb_to_render_color_premultiplies() {
        // Fully transparent → all components zero regardless of color.
        assert_eq!(argb_to_render_color(0x00FF00FF), (0, 0, 0, 0));
    }
}
