//! Decode-only source maps: JSON + base64-VLQ mappings, both lookup directions, nothing else.
//! In-house because the format is small and the needs are narrow — forward lookup resolves
//! error stacks to original files, reverse lookup turns `src/cart.js:5` into a breakpoint site.
//! Index maps (`sections`) are refused honestly until a real bundle needs them.

use serde::Deserialize;

/// Decoded-map size guards — a pathological map should fail loudly, not own the daemon's memory.
/// The line cap matters independently of the byte cap: a megabyte of bare `;` would otherwise
/// decode into tens of millions of empty line vectors.
const MAX_MAP_BYTES: usize = 64 * 1024 * 1024;
const MAX_MAP_LINES: usize = 2_000_000;

/// Retained `sourcesContent` is a convenience (line snippets), not a requirement — past this
/// total it is dropped rather than letting one giant map own the daemon's memory.
const MAX_CONTENT_BYTES: usize = 16 * 1024 * 1024;

/// One parsed source map: original source paths and the generated↔original line/column mesh.
pub struct SourceMap {
    sources: Vec<String>,
    /// Original file contents per source, when the map embedded them (and they fit the cap).
    contents: Vec<Option<String>>,
    /// Per generated line, segments sorted by generated column.
    lines: Vec<Vec<Segment>>,
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    gen_col: u32,
    src: u32,
    src_line: u32,
    src_col: u32,
}

/// How a user path matched a map's sources.
#[derive(Debug)]
pub enum SourceMatch {
    None,
    One(usize),
    /// Several distinct sources end with the path — the caller lists them and asks for more.
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct RawMap {
    version: u32,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default, rename = "sourceRoot")]
    source_root: Option<String>,
    #[serde(default, rename = "sourcesContent")]
    sources_content: Vec<Option<String>>,
    #[serde(default)]
    mappings: String,
    #[serde(default)]
    sections: Option<serde_json::Value>,
}

impl SourceMap {
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_MAP_BYTES {
            return Err(format!("source map too large ({} bytes)", text.len()));
        }
        let raw: RawMap = serde_json::from_str(text).map_err(|error| error.to_string())?;
        if raw.sections.is_some() {
            return Err("index maps (sections) are not supported yet".to_owned());
        }
        if raw.version != 3 {
            return Err(format!("unsupported source map version {}", raw.version));
        }
        let root = raw.source_root.unwrap_or_default();
        let sources: Vec<String> = raw
            .sources
            .into_iter()
            .map(|source| {
                if root.is_empty() {
                    source
                } else {
                    format!("{}/{}", root.trim_end_matches('/'), source.trim_start_matches('/'))
                }
            })
            .collect();
        let mut contents = raw.sources_content;
        contents.resize(sources.len(), None);
        let total: usize = contents.iter().flatten().map(String::len).sum();
        if total > MAX_CONTENT_BYTES {
            contents = vec![None; sources.len()];
        }
        let lines = decode_mappings(&raw.mappings)?;
        Ok(Self { sources, contents, lines })
    }

    /// One line of an original source, from embedded `sourcesContent` — the proof of what code
    /// a mapped location points at. `line` is 0-based.
    pub fn source_line(&self, source: usize, line: u32) -> Option<&str> {
        self.contents.get(source)?.as_deref()?.lines().nth(line as usize)
    }

    /// Match a user path against the sources: exact (after normalization) beats path-suffix;
    /// several distinct suffix matches are ambiguity, not a guess.
    pub fn match_source(&self, path: &str) -> SourceMatch {
        let wanted = normalize(path);
        let mut suffix_hits: Vec<usize> = Vec::new();
        for (index, source) in self.sources.iter().enumerate() {
            let normalized = normalize(source);
            if normalized == wanted {
                return SourceMatch::One(index);
            }
            if normalized.ends_with(&wanted)
                && normalized[..normalized.len() - wanted.len()].ends_with('/')
            {
                suffix_hits.push(index);
            }
        }
        match suffix_hits.as_slice() {
            [] => SourceMatch::None,
            [index] => SourceMatch::One(*index),
            _ => SourceMatch::Many(
                suffix_hits.iter().map(|&index| self.sources[index].clone()).collect(),
            ),
        }
    }

    /// The generated site for an original line: the first segment (lowest generated line, then
    /// column) mapping to it. The column matters — in a minified bundle the whole file is one
    /// generated line, and a column-less breakpoint there lands at the start of the bundle.
    pub fn generated_for(&self, source: usize, line: u32) -> Option<(u32, u32)> {
        let mut best: Option<(u32, u32)> = None;
        for (gen_line, segments) in self.lines.iter().enumerate() {
            for segment in segments {
                if segment.src == source as u32 && segment.src_line == line {
                    let site = (gen_line as u32, segment.gen_col);
                    if best.is_none_or(|current| site < current) {
                        best = Some(site);
                    }
                }
            }
        }
        best
    }

    /// The original location covering a generated position: the last segment at or before the
    /// column on that line. Returns `(source_path, line, column)`, all 0-based. Binary search —
    /// a minified bundle is one generated line with 10⁵+ segments, and this runs per frame per
    /// query under the daemon's lock.
    pub fn original_for(&self, line: u32, column: u32) -> Option<(&str, u32, u32)> {
        let segments = self.lines.get(line as usize)?;
        let at_or_before = segments.partition_point(|segment| segment.gen_col <= column);
        let segment = segments.get(at_or_before.checked_sub(1)?)?;
        let source = self.sources.get(segment.src as usize)?;
        Some((source, segment.src_line, segment.src_col))
    }
}

/// Strip the bundler chrome that makes equal paths look different: `webpack://ns/`, `file://`,
/// and leading `./`.
fn normalize(path: &str) -> String {
    let path = path.strip_prefix("webpack://").map_or(path, |rest| {
        // webpack://<namespace>/<path> — the namespace is not part of any user path.
        rest.split_once('/').map_or(rest, |(_, path)| path)
    });
    let path = path.strip_prefix("file://").unwrap_or(path);
    path.trim_start_matches("./").to_owned()
}

/// Resolve a (possibly relative) source-map URL against its script's URL.
pub fn resolve_map_url(script_url: &str, map_url: &str) -> String {
    if map_url.contains("://") || map_url.starts_with("data:") {
        return map_url.to_owned();
    }
    match script_url.rsplit_once('/') {
        Some((base, _)) => format!("{base}/{map_url}"),
        None => map_url.to_owned(),
    }
}

/// Extract the JSON from an inline `data:` source-map URL, if that is what this is.
pub fn inline_map(map_url: &str) -> Option<Result<String, String>> {
    let rest = map_url.strip_prefix("data:")?;
    let (header, payload) = rest.split_once(',')?;
    if header.contains("base64") {
        Some(
            base64_decode(payload)
                .and_then(|bytes| String::from_utf8(bytes).map_err(|_| "not utf-8".to_owned())),
        )
    } else {
        // Percent-encoded data URLs are rare for maps; refuse rather than mis-decode.
        Some(Err("non-base64 data: source map".to_owned()))
    }
}

fn decode_mappings(mappings: &str) -> Result<Vec<Vec<Segment>>, String> {
    let mut lines = Vec::new();
    // src/src_line/src_col carry across lines; gen_col resets per line.
    let (mut src, mut src_line, mut src_col) = (0i64, 0i64, 0i64);

    for group in mappings.split(';') {
        if lines.len() >= MAX_MAP_LINES {
            return Err(format!("source map exceeds {MAX_MAP_LINES} generated lines"));
        }
        let mut segments = Vec::new();
        let mut gen_col = 0i64;
        for raw in group.split(',').filter(|raw| !raw.is_empty()) {
            let fields = vlq_decode(raw)?;
            match fields.as_slice() {
                // 1-field segments map generated code to nothing — no use to us.
                [col_delta] => gen_col += col_delta,
                [col_delta, src_delta, line_delta, col_src_delta]
                | [col_delta, src_delta, line_delta, col_src_delta, _] => {
                    gen_col += col_delta;
                    src += src_delta;
                    src_line += line_delta;
                    src_col += col_src_delta;
                    if gen_col < 0 || src < 0 || src_line < 0 || src_col < 0 {
                        return Err("negative position in mappings".to_owned());
                    }
                    segments.push(Segment {
                        gen_col: gen_col as u32,
                        src: src as u32,
                        src_line: src_line as u32,
                        src_col: src_col as u32,
                    });
                }
                _ => return Err(format!("malformed mapping segment '{raw}'")),
            }
        }
        // The spec permits negative column deltas — `original_for`'s binary search needs order.
        segments.sort_unstable_by_key(|segment| segment.gen_col);
        lines.push(segments);
    }
    Ok(lines)
}

/// Decode one comma-free VLQ segment into its fields.
fn vlq_decode(segment: &str) -> Result<Vec<i64>, String> {
    let mut fields = Vec::new();
    let mut value: i64 = 0;
    let mut shift = 0u32;
    for ch in segment.chars() {
        let digit = base64_value(ch).ok_or_else(|| format!("bad VLQ char '{ch}'"))?;
        value |= i64::from(digit & 0x1f) << shift;
        if digit & 0x20 != 0 {
            shift += 5;
            if shift > 60 {
                return Err("VLQ overflow".to_owned());
            }
            continue;
        }
        let signed = if value & 1 != 0 { -(value >> 1) } else { value >> 1 };
        fields.push(signed);
        value = 0;
        shift = 0;
    }
    if shift != 0 {
        return Err("truncated VLQ segment".to_owned());
    }
    Ok(fields)
}

fn base64_value(ch: char) -> Option<u8> {
    match ch {
        'A'..='Z' => Some(ch as u8 - b'A'),
        'a'..='z' => Some(ch as u8 - b'a' + 26),
        '0'..='9' => Some(ch as u8 - b'0' + 52),
        '+' => Some(62),
        '/' => Some(63),
        _ => None,
    }
}

fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for ch in text.chars().filter(|ch| *ch != '=') {
        let value = base64_value(ch).ok_or_else(|| format!("bad base64 char '{ch}'"))?;
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `bundle.js` line 0 has two segments: col 0 → cart.js 0:0, col 4 → cart.js 2:0; line 1
    /// maps col 0 → cart.js 3:0. (`AAAA`=[0,0,0,0], `IAEA`=[4,0,2,0], `AACA`=[0,0,1,0].)
    fn fixture() -> SourceMap {
        SourceMap::parse(r#"{"version":3,"sources":["src/cart.js"],"mappings":"AAAA,IAEA;AACA"}"#)
            .unwrap()
    }

    #[test]
    fn mappings_decode_in_both_directions() {
        let map = fixture();
        assert_eq!(map.original_for(0, 0), Some(("src/cart.js", 0, 0)));
        assert_eq!(map.original_for(0, 4), Some(("src/cart.js", 2, 0)));
        // Between segments, the covering segment is the one before.
        assert_eq!(map.original_for(0, 3), Some(("src/cart.js", 0, 0)));
        assert_eq!(map.original_for(1, 9), Some(("src/cart.js", 3, 0)));
        assert_eq!(map.original_for(7, 0), None);

        assert_eq!(map.generated_for(0, 2), Some((0, 4)));
        assert_eq!(map.generated_for(0, 3), Some((1, 0)));
        assert_eq!(map.generated_for(0, 9), None);
    }

    #[test]
    fn source_matching_is_exact_then_suffix_and_names_ambiguity() {
        let map = SourceMap::parse(
            r#"{"version":3,"sources":["webpack://app/./src/cart.js","src/checkout/cart.js"],"mappings":""}"#,
        )
        .unwrap();
        assert!(matches!(map.match_source("src/cart.js"), SourceMatch::One(0)));
        assert!(matches!(map.match_source("checkout/cart.js"), SourceMatch::One(1)));
        assert!(matches!(map.match_source("cart.js"), SourceMatch::Many(both) if both.len() == 2));
        assert!(matches!(map.match_source("nope.js"), SourceMatch::None));
        // A suffix only matches at a path-segment boundary.
        assert!(matches!(map.match_source("art.js"), SourceMatch::None));
    }

    #[test]
    fn negative_and_garbage_mappings_fail_loudly() {
        assert!(SourceMap::parse(r#"{"version":3,"sources":[],"mappings":"!!!"}"#).is_err());
        assert!(SourceMap::parse(r#"{"version":2,"sources":[],"mappings":""}"#).is_err());
        assert!(SourceMap::parse(r#"{"version":3,"sections":[],"mappings":""}"#).is_err());
        // [0,0,-1,0] walks src_line negative.
        assert!(SourceMap::parse(r#"{"version":3,"sources":["a"],"mappings":"AADA"}"#).is_err());
    }

    /// The spec permits negative column deltas, so segments can arrive out of generated order —
    /// `IAEA` = [+4,0,+2,0] (col 4 → a.js:2), then `JACA` = [-4,0,+1,0] (col 0 → a.js:3).
    #[test]
    fn out_of_order_segments_sort_so_lookup_stays_correct() {
        let map =
            SourceMap::parse(r#"{"version":3,"sources":["a.js"],"mappings":"IAEA,JACA"}"#).unwrap();
        assert_eq!(map.original_for(0, 0), Some(("a.js", 3, 0)));
        assert_eq!(map.original_for(0, 2), Some(("a.js", 3, 0)));
        assert_eq!(map.original_for(0, 4), Some(("a.js", 2, 0)));
    }

    #[test]
    fn source_line_reads_embedded_content() {
        let map = SourceMap::parse(
            r#"{"version":3,"sources":["src/cart.js"],"sourcesContent":["const a = 1;\nreturn;\nconst b = 2;"],"mappings":"AAAA"}"#,
        )
        .unwrap();
        assert_eq!(map.source_line(0, 1), Some("return;"));
        assert_eq!(map.source_line(0, 9), None);
        assert_eq!(map.source_line(3, 0), None);
        let bare = SourceMap::parse(r#"{"version":3,"sources":["a.js"],"mappings":""}"#).unwrap();
        assert_eq!(bare.source_line(0, 0), None);
    }

    #[test]
    fn source_root_and_url_resolution_compose() {
        let map = SourceMap::parse(
            r#"{"version":3,"sourceRoot":"webpack://app","sources":["src/a.js"],"mappings":""}"#,
        )
        .unwrap();
        assert!(matches!(map.match_source("src/a.js"), SourceMatch::One(0)));

        assert_eq!(
            resolve_map_url("file:///app/dist/bundle.js", "bundle.js.map"),
            "file:///app/dist/bundle.js.map"
        );
        assert_eq!(resolve_map_url("file:///x.js", "https://cdn/x.map"), "https://cdn/x.map");
        let inline = inline_map("data:application/json;base64,eyJ2ZXJzaW9uIjozfQ==").unwrap();
        assert_eq!(inline.unwrap(), r#"{"version":3}"#);
    }
}
