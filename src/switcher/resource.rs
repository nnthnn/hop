//! RAII guards for short-lived X11 resources used during tile rendering.
//!
//! Each guard frees its resource on drop, so an early `?` return inside a draw
//! helper can't leak a pixmap, GC, or XRender picture. This matters now that the
//! event loop recovers from transient errors and keeps running: without the
//! guards, every recovered mid-draw error would leak server resources for the
//! life of the daemon. Cleanup is best-effort — the free request is queued and any
//! error ignored (the server also reclaims everything on disconnect).

use std::error::Error;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, CreateGCAux};
use x11rb::protocol::render::{ConnectionExt as _, CreatePictureAux};
use x11rb::rust_connection::RustConnection;

/// A pixmap freed with `free_pixmap` on drop.
pub(super) struct PixmapGuard<'c> {
    conn: &'c RustConnection,
    pub id: u32,
}

impl<'c> PixmapGuard<'c> {
    pub fn create(conn: &'c RustConnection, depth: u8, drawable: u32, w: u16, h: u16)
        -> Result<Self, Box<dyn Error>>
    {
        let id = conn.generate_id()?;
        conn.create_pixmap(depth, id, drawable, w, h)?;
        Ok(Self { conn, id })
    }
}

impl Drop for PixmapGuard<'_> {
    fn drop(&mut self) {
        let _ = self.conn.free_pixmap(self.id);
    }
}

/// A graphics context freed with `free_gc` on drop.
pub(super) struct GcGuard<'c> {
    conn: &'c RustConnection,
    pub id: u32,
}

impl<'c> GcGuard<'c> {
    pub fn create(conn: &'c RustConnection, drawable: u32, aux: &CreateGCAux)
        -> Result<Self, Box<dyn Error>>
    {
        let id = conn.generate_id()?;
        conn.create_gc(id, drawable, aux)?;
        Ok(Self { conn, id })
    }
}

impl Drop for GcGuard<'_> {
    fn drop(&mut self) {
        let _ = self.conn.free_gc(self.id);
    }
}

/// An XRender picture freed with `render_free_picture` on drop.
pub(super) struct PictureGuard<'c> {
    conn: &'c RustConnection,
    pub id: u32,
}

impl<'c> PictureGuard<'c> {
    pub fn create(conn: &'c RustConnection, drawable: u32, format: u32, aux: &CreatePictureAux)
        -> Result<Self, Box<dyn Error>>
    {
        let id = conn.generate_id()?;
        conn.render_create_picture(id, drawable, format, aux)?;
        Ok(Self { conn, id })
    }
}

impl Drop for PictureGuard<'_> {
    fn drop(&mut self) {
        let _ = self.conn.render_free_picture(self.id);
    }
}
