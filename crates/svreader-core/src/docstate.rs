//! koreader-compatible `.sdr/<name>/metadata.lua` reader/writer.
//!
//! We don't ship a real Lua interpreter. We only need a narrow subset
//! of the format koreader actually writes: a single `return { ... }`
//! expression whose leaves are strings / numbers / booleans / nested
//! tables. Unknown keys round-trip verbatim so we don't destroy
//! annotations/bookmarks written by koreader itself.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::viewport::{Rotation, ZoomMode};

#[derive(Debug, Clone, PartialEq)]
pub enum LuaValue {
    Nil,
    Bool(bool),
    Number(f64),
    String(String),
    Table(LuaTable),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LuaTable {
    /// Preserves koreader-style "named" keys.
    pub entries: Vec<(LuaKey, LuaValue)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LuaKey {
    String(String),
    Int(i64),
}

impl LuaTable {
    pub fn get(&self, key: &str) -> Option<&LuaValue> {
        self.entries
            .iter()
            .find(|(k, _)| matches!(k, LuaKey::String(s) if s == key))
            .map(|(_, v)| v)
    }

    pub fn set(&mut self, key: &str, value: LuaValue) {
        for (k, v) in self.entries.iter_mut() {
            if matches!(k, LuaKey::String(s) if s == key) {
                *v = value;
                return;
            }
        }
        self.entries.push((LuaKey::String(key.to_string()), value));
    }

    pub fn remove(&mut self, key: &str) {
        self.entries
            .retain(|(k, _)| !matches!(k, LuaKey::String(s) if s == key));
    }
}

/// One persisted bookmark. Vim-style — single-letter mark + the spot
/// on the page the user was looking at when they set it (page index
/// plus current scroll offsets, so jumping back lands at the same
/// view).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bookmark {
    pub mark: char,
    pub page: usize,
    pub x_off: i32,
    pub y_off: i32,
}

/// Persistent per-document reader state.
#[derive(Debug, Clone)]
pub struct DocState {
    pub last_page: usize,
    pub zoom: ZoomMode,
    pub rotation: Rotation,
    pub scroll_x: i32,
    pub scroll_y: i32,
    pub night_mode: bool,
    pub render_dpi: Option<f32>,
    pub render_quality: f32,
    pub cache_enabled: bool,
    /// RenderCache capacity the user wants. `None` → use the
    /// default. Last-loaded PDF's value wins because the caches are
    /// workspace-global.
    pub cache_size: Option<usize>,
    /// ECache (encoded-frame cache) capacity.
    pub ecache_size: Option<usize>,
    /// Per-document marks set with `m{a-z}` and recalled with
    /// `'{a-z}`. Stored as a list (not a map) so the on-disk layout
    /// stays Lua-array-of-tables, simple to round-trip.
    pub bookmarks: Vec<Bookmark>,
    /// Whether mouse capture is enabled. Persists across sessions so
    /// users who turn it off don't keep losing terminal text-select
    /// every time they reopen a file. None = use the global default.
    pub mouse_enabled: Option<bool>,
    /// Extra keys we don't understand, preserved verbatim so we don't
    /// trample koreader-only fields like bookmarks/annotations.
    extras: LuaTable,
}

impl Default for DocState {
    fn default() -> Self {
        Self {
            last_page: 0,
            zoom: ZoomMode::FitWidth,
            rotation: Rotation::R0,
            scroll_x: 0,
            scroll_y: 0,
            night_mode: false,
            render_dpi: None,
            render_quality: 1.0,
            cache_enabled: true,
            cache_size: None,
            ecache_size: None,
            bookmarks: Vec::new(),
            mouse_enabled: None,
            extras: LuaTable::default(),
        }
    }
}

impl DocState {
    /// Set or replace a single-letter mark. Replaces an existing mark
    /// of the same letter rather than appending a duplicate.
    pub fn set_bookmark(&mut self, mark: char, page: usize, x_off: i32, y_off: i32) {
        let bm = Bookmark {
            mark,
            page,
            x_off,
            y_off,
        };
        if let Some(slot) = self.bookmarks.iter_mut().find(|b| b.mark == mark) {
            *slot = bm;
        } else {
            self.bookmarks.push(bm);
        }
    }

    pub fn delete_bookmark(&mut self, mark: char) -> bool {
        let before = self.bookmarks.len();
        self.bookmarks.retain(|b| b.mark != mark);
        before != self.bookmarks.len()
    }

    pub fn find_bookmark(&self, mark: char) -> Option<&Bookmark> {
        self.bookmarks.iter().find(|b| b.mark == mark)
    }
}

impl DocState {
    /// Location where we save metadata for a given PDF path. koreader
    /// writes `<file>.sdr/metadata.pdf.lua` (suffix matches the doc
    /// extension). We match that.
    pub fn sidecar_path(pdf_path: &Path) -> PathBuf {
        let stem = pdf_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".into());
        let parent = pdf_path.parent().unwrap_or_else(|| Path::new("."));
        let sdr_dir = parent.join(format!("{stem}.sdr"));
        let ext = pdf_path
            .extension()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_else(|| "pdf".into());
        sdr_dir.join(format!("metadata.{ext}.lua"))
    }

    pub fn load(pdf_path: &Path) -> Result<Self> {
        let path = Self::sidecar_path(pdf_path);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading sidecar {:?}", path))?;
        let tbl = parse_lua_return_table(&text)
            .with_context(|| format!("parsing sidecar {:?}", path))?;
        Ok(Self::from_table(tbl))
    }

    pub fn save(&self, pdf_path: &Path) -> Result<()> {
        let path = Self::sidecar_path(pdf_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating sidecar dir {:?}", parent))?;
        }
        let mut tbl = self.extras.clone();
        self.merge_into(&mut tbl);
        let mut out = String::new();
        out.push_str("-- ");
        out.push_str(&path.to_string_lossy());
        out.push_str("\nreturn ");
        write_lua_table(&tbl, 0, &mut out);
        out.push('\n');
        fs::write(&path, out)
            .with_context(|| format!("writing sidecar {:?}", path))?;
        Ok(())
    }

    fn from_table(tbl: LuaTable) -> Self {
        let mut st = Self::default();
        for (k, v) in &tbl.entries {
            let key = match k {
                LuaKey::String(s) => s.as_str(),
                _ => continue,
            };
            match (key, v) {
                ("last_page", LuaValue::Number(n)) => st.last_page = (*n as i64).max(0) as usize,
                ("zoom", LuaValue::String(s)) => st.zoom = parse_zoom(s).unwrap_or(st.zoom),
                ("zoom_mult", LuaValue::Number(n)) => {
                    // If zoom was Custom, remember the multiplier.
                    if matches!(st.zoom, ZoomMode::Custom(_)) {
                        st.zoom = ZoomMode::Custom(*n as f32);
                    }
                }
                ("rotation", LuaValue::Number(n)) => {
                    st.rotation = Rotation::from_degrees(*n as i32);
                }
                ("scroll_x", LuaValue::Number(n)) => st.scroll_x = *n as i32,
                ("scroll_y", LuaValue::Number(n)) => st.scroll_y = *n as i32,
                ("night_mode", LuaValue::Bool(b)) => st.night_mode = *b,
                ("render_dpi", LuaValue::Number(n)) => st.render_dpi = Some(*n as f32),
                ("render_dpi", LuaValue::Nil) => st.render_dpi = None,
                ("render_quality", LuaValue::Number(n)) => st.render_quality = *n as f32,
                ("cache_enabled", LuaValue::Bool(b)) => st.cache_enabled = *b,
                ("cache_size", LuaValue::Number(n)) => {
                    let v = (*n as i64).max(1) as usize;
                    st.cache_size = Some(v);
                }
                ("ecache_size", LuaValue::Number(n)) => {
                    let v = (*n as i64).max(1) as usize;
                    st.ecache_size = Some(v);
                }
                ("mouse_enabled", LuaValue::Bool(b)) => st.mouse_enabled = Some(*b),
                ("svr_bookmarks", LuaValue::Table(bt)) => {
                    for (_, entry) in &bt.entries {
                        let LuaValue::Table(et) = entry else { continue };
                        let mark = match et.get("mark") {
                            Some(LuaValue::String(s)) if !s.is_empty() => {
                                s.chars().next().unwrap()
                            }
                            _ => continue,
                        };
                        let page = match et.get("page") {
                            Some(LuaValue::Number(n)) => (*n as i64).max(0) as usize,
                            _ => continue,
                        };
                        let x_off = match et.get("x_off") {
                            Some(LuaValue::Number(n)) => *n as i32,
                            _ => 0,
                        };
                        let y_off = match et.get("y_off") {
                            Some(LuaValue::Number(n)) => *n as i32,
                            _ => 0,
                        };
                        st.set_bookmark(mark, page, x_off, y_off);
                    }
                }
                _ => {}
            }
        }
        let mut extras = tbl;
        for key in [
            "last_page",
            "zoom",
            "zoom_mult",
            "rotation",
            "scroll_x",
            "scroll_y",
            "night_mode",
            "render_dpi",
            "render_quality",
            "cache_enabled",
            "cache_size",
            "ecache_size",
            "mouse_enabled",
            "svr_bookmarks",
        ] {
            extras.remove(key);
        }
        st.extras = extras;
        st
    }

    fn merge_into(&self, tbl: &mut LuaTable) {
        tbl.set("last_page", LuaValue::Number(self.last_page as f64));
        match self.zoom {
            ZoomMode::FitWidth => tbl.set("zoom", LuaValue::String("fit-w".into())),
            ZoomMode::FitHeight => tbl.set("zoom", LuaValue::String("fit-h".into())),
            ZoomMode::FitPage => tbl.set("zoom", LuaValue::String("fit-p".into())),
            ZoomMode::Custom(m) => {
                tbl.set("zoom", LuaValue::String("custom".into()));
                tbl.set("zoom_mult", LuaValue::Number(m as f64));
            }
        }
        tbl.set("rotation", LuaValue::Number(self.rotation.degrees() as f64));
        tbl.set("scroll_x", LuaValue::Number(self.scroll_x as f64));
        tbl.set("scroll_y", LuaValue::Number(self.scroll_y as f64));
        tbl.set("night_mode", LuaValue::Bool(self.night_mode));
        match self.render_dpi {
            Some(d) => tbl.set("render_dpi", LuaValue::Number(d as f64)),
            None => tbl.remove("render_dpi"),
        }
        tbl.set("render_quality", LuaValue::Number(self.render_quality as f64));
        tbl.set("cache_enabled", LuaValue::Bool(self.cache_enabled));
        match self.cache_size {
            Some(n) => tbl.set("cache_size", LuaValue::Number(n as f64)),
            None => tbl.remove("cache_size"),
        }
        match self.ecache_size {
            Some(n) => tbl.set("ecache_size", LuaValue::Number(n as f64)),
            None => tbl.remove("ecache_size"),
        }
        match self.mouse_enabled {
            Some(b) => tbl.set("mouse_enabled", LuaValue::Bool(b)),
            None => tbl.remove("mouse_enabled"),
        }
        if self.bookmarks.is_empty() {
            tbl.remove("svr_bookmarks");
        } else {
            let mut arr = LuaTable::default();
            for (i, bm) in self.bookmarks.iter().enumerate() {
                let mut bt = LuaTable::default();
                bt.set("mark", LuaValue::String(bm.mark.to_string()));
                bt.set("page", LuaValue::Number(bm.page as f64));
                bt.set("x_off", LuaValue::Number(bm.x_off as f64));
                bt.set("y_off", LuaValue::Number(bm.y_off as f64));
                arr.entries
                    .push((LuaKey::Int(i as i64 + 1), LuaValue::Table(bt)));
            }
            tbl.set("svr_bookmarks", LuaValue::Table(arr));
        }
    }
}

fn parse_zoom(s: &str) -> Option<ZoomMode> {
    match s {
        "fit-w" | "fitwidth" | "fit_width" => Some(ZoomMode::FitWidth),
        "fit-h" | "fitheight" | "fit_height" => Some(ZoomMode::FitHeight),
        "fit-p" | "fitpage" | "fit_page" => Some(ZoomMode::FitPage),
        "custom" => Some(ZoomMode::Custom(1.0)),
        _ => None,
    }
}

// ------------------------------ Parser ------------------------------

pub fn parse_lua_return_table(src: &str) -> Result<LuaTable> {
    let mut p = Parser::new(src);
    p.skip_ws_and_comments();
    p.expect_keyword("return")?;
    p.skip_ws_and_comments();
    let value = p.parse_value()?;
    p.skip_ws_and_comments();
    match value {
        LuaValue::Table(t) => Ok(t),
        _ => Err(anyhow!("top-level value is not a table")),
    }
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn eat(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn starts_with(&self, s: &str) -> bool {
        self.src[self.pos..].starts_with(s.as_bytes())
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            while let Some(b) = self.peek() {
                if b.is_ascii_whitespace() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            if self.starts_with("--") {
                // Line comment or block comment.
                if self.src[self.pos..].starts_with(b"--[[") {
                    self.pos += 4;
                    while self.pos < self.src.len() && !self.starts_with("]]") {
                        self.pos += 1;
                    }
                    if self.starts_with("]]") {
                        self.pos += 2;
                    }
                } else {
                    while let Some(b) = self.peek() {
                        self.pos += 1;
                        if b == b'\n' {
                            break;
                        }
                    }
                }
            } else {
                break;
            }
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<()> {
        if self.starts_with(kw) {
            self.pos += kw.len();
            Ok(())
        } else {
            Err(anyhow!(
                "expected keyword {kw:?} at offset {}",
                self.pos
            ))
        }
    }

    fn parse_value(&mut self) -> Result<LuaValue> {
        self.skip_ws_and_comments();
        let b = self
            .peek()
            .ok_or_else(|| anyhow!("unexpected EOF parsing value at {}", self.pos))?;
        match b {
            b'{' => self.parse_table().map(LuaValue::Table),
            b'"' | b'\'' => self.parse_string().map(LuaValue::String),
            b'-' | b'0'..=b'9' => self.parse_number().map(LuaValue::Number),
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let ident = self.parse_ident()?;
                match ident.as_str() {
                    "true" => Ok(LuaValue::Bool(true)),
                    "false" => Ok(LuaValue::Bool(false)),
                    "nil" => Ok(LuaValue::Nil),
                    other => Err(anyhow!("unexpected identifier {other:?}")),
                }
            }
            _ => Err(anyhow!(
                "unexpected byte {:?} at offset {}",
                b as char,
                self.pos
            )),
        }
    }

    fn parse_string(&mut self) -> Result<String> {
        let quote = self.eat().unwrap();
        let mut out = String::new();
        while let Some(b) = self.eat() {
            if b == quote {
                return Ok(out);
            }
            if b == b'\\' {
                let esc = self
                    .eat()
                    .ok_or_else(|| anyhow!("bad escape at end of file"))?;
                match esc {
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'\\' => out.push('\\'),
                    b'\'' => out.push('\''),
                    b'"' => out.push('"'),
                    b'0' => out.push('\0'),
                    b'x' => {
                        let h1 = self.eat().ok_or_else(|| anyhow!("bad \\x"))?;
                        let h2 = self.eat().ok_or_else(|| anyhow!("bad \\x"))?;
                        let v = u8::from_str_radix(
                            std::str::from_utf8(&[h1, h2]).unwrap_or("00"),
                            16,
                        )
                        .unwrap_or(0);
                        out.push(v as char);
                    }
                    d if d.is_ascii_digit() => {
                        // Numeric escape: up to three digits.
                        let mut n = (d - b'0') as u32;
                        for _ in 0..2 {
                            if let Some(b2) = self.peek() {
                                if b2.is_ascii_digit() {
                                    n = n * 10 + (b2 - b'0') as u32;
                                    self.pos += 1;
                                    continue;
                                }
                            }
                            break;
                        }
                        if let Some(c) = char::from_u32(n) {
                            out.push(c);
                        }
                    }
                    other => out.push(other as char),
                }
                continue;
            }
            out.push(b as char);
        }
        Err(anyhow!("unterminated string"))
    }

    fn parse_number(&mut self) -> Result<f64> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() || b == b'.' || b == b'e' || b == b'E' || b == b'+' || b == b'-'
            {
                self.pos += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.src[start..self.pos])
            .map_err(|_| anyhow!("invalid utf8 in number"))?;
        s.parse::<f64>()
            .map_err(|e| anyhow!("bad number {s:?}: {e}"))
    }

    fn parse_ident(&mut self) -> Result<String> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.src[start..self.pos])
            .map_err(|_| anyhow!("invalid utf8 in ident"))?;
        Ok(s.to_string())
    }

    fn parse_table(&mut self) -> Result<LuaTable> {
        self.expect_keyword("{")?;
        let mut tbl = LuaTable::default();
        let mut next_int: i64 = 1;
        loop {
            self.skip_ws_and_comments();
            if let Some(b) = self.peek() {
                if b == b'}' {
                    self.pos += 1;
                    break;
                }
            } else {
                return Err(anyhow!("unterminated table"));
            }
            // Entry: [key] = value  |  key = value  |  value
            let key: LuaKey;
            let save_pos = self.pos;
            let mut had_key = false;
            if self.peek() == Some(b'[') {
                self.pos += 1;
                self.skip_ws_and_comments();
                let k = self.parse_value()?;
                self.skip_ws_and_comments();
                self.expect_keyword("]")?;
                self.skip_ws_and_comments();
                self.expect_keyword("=")?;
                key = match k {
                    LuaValue::String(s) => LuaKey::String(s),
                    LuaValue::Number(n) => LuaKey::Int(n as i64),
                    _ => return Err(anyhow!("unsupported table key type")),
                };
                had_key = true;
            } else if matches!(self.peek(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_')) {
                let ident = self.parse_ident()?;
                self.skip_ws_and_comments();
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    key = LuaKey::String(ident);
                    had_key = true;
                } else {
                    // Not a key — rewind and treat as a value.
                    self.pos = save_pos;
                    key = LuaKey::Int(next_int);
                    next_int += 1;
                }
            } else {
                key = LuaKey::Int(next_int);
                next_int += 1;
            }
            self.skip_ws_and_comments();
            let value = self.parse_value()?;
            tbl.entries.push((key, value));
            let _ = had_key;
            self.skip_ws_and_comments();
            if self.peek() == Some(b',') || self.peek() == Some(b';') {
                self.pos += 1;
            } else if self.peek() == Some(b'}') {
                self.pos += 1;
                break;
            } else {
                return Err(anyhow!(
                    "expected ',' or '}}' at offset {} (got {:?})",
                    self.pos,
                    self.peek().map(|b| b as char)
                ));
            }
        }
        Ok(tbl)
    }
}

// ------------------------------ Writer ------------------------------

fn write_lua_table(tbl: &LuaTable, indent: usize, out: &mut String) {
    if tbl.entries.is_empty() {
        out.push_str("{}");
        return;
    }
    out.push('{');
    for (k, v) in &tbl.entries {
        out.push('\n');
        for _ in 0..indent + 1 {
            out.push_str("    ");
        }
        out.push('[');
        match k {
            LuaKey::String(s) => {
                out.push('"');
                for c in s.chars() {
                    escape_char(c, out);
                }
                out.push('"');
            }
            LuaKey::Int(n) => {
                out.push_str(&n.to_string());
            }
        }
        out.push_str("] = ");
        write_lua_value(v, indent + 1, out);
        out.push(',');
    }
    out.push('\n');
    for _ in 0..indent {
        out.push_str("    ");
    }
    out.push('}');
}

fn write_lua_value(v: &LuaValue, indent: usize, out: &mut String) {
    match v {
        LuaValue::Nil => out.push_str("nil"),
        LuaValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        LuaValue::Number(n) => {
            if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e15 {
                out.push_str(&format!("{}", *n as i64));
            } else {
                out.push_str(&format!("{}", n));
            }
        }
        LuaValue::String(s) => {
            out.push('"');
            for c in s.chars() {
                escape_char(c, out);
            }
            out.push('"');
        }
        LuaValue::Table(t) => write_lua_table(t, indent, out),
    }
}

fn escape_char(c: char, out: &mut String) {
    match c {
        '\\' => out.push_str("\\\\"),
        '"' => out.push_str("\\\""),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        c if (c as u32) < 0x20 => out.push_str(&format!("\\{}", c as u32)),
        c => out.push(c),
    }
}
