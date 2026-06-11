//! Pure trace machinery: the in-page JS templates that install/restore/probe fn wrappers, and
//! the decoder for the `__kit_trace__` binding payloads they emit. No I/O — the daemon supplies
//! the session and the wire; everything here takes data and returns data, so the injection
//! safety and the decode bounds are unit-testable with zero sockets.

use serde::Deserialize;

use crate::cdp::TraceOutcome;

/// Per-trace emission cap unit — hits per second past which the page counts instead of sends.
pub const RATE_FLOOR: u64 = 1;
pub const RATE_CEIL: u64 = 1_000;
/// Defense-in-depth caps on binding payloads — the page already bounds previews, but the binding
/// is callable by page code, so the daemon re-bounds everything it decodes.
const PAYLOAD_CAP: usize = 8 * 1024;
const PREVIEW_CAP: usize = 240;

/// A trace name doubles as the in-page registry key and the Timeline row label: short,
/// shell-friendly, and safe to embed anywhere.
pub fn validate_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 48
        && name.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));
    if valid {
        Ok(())
    } else {
        Err(format!("invalid trace name '{name}' — ASCII letters, digits, '-', '_', '.' (max 48)"))
    }
}

/// Split and validate a dotted function path. The path is the one thing spliced into page JS as
/// *code* (it must be — it names live objects), so it is restricted to identifier segments; that
/// restriction is what makes the splice injection-proof.
pub fn parse_fn_path(path: &str) -> Result<Vec<String>, String> {
    let segments: Vec<String> = path.split('.').map(str::to_owned).collect();
    for segment in &segments {
        let mut chars = segment.chars();
        let head_ok =
            chars.next().is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_' || ch == '$');
        let tail_ok = chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$');
        if !head_ok || !tail_ok {
            return Err(format!(
                "invalid path '{path}' — dotted identifiers only (e.g. app.api.save)"
            ));
        }
        if segment.starts_with("__kit") {
            return Err("kit's own instrumentation cannot be traced".to_owned());
        }
    }
    Ok(segments)
}

/// Default trace name for a fn path: its last segment.
pub fn default_fn_name(path: &str) -> String {
    path.rsplit('.').next().unwrap_or(path).to_owned()
}

/// A logpoint site: a script URL (suffix or absolute) plus a 1-based line and optional column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointLocation {
    pub url: String,
    pub line: u32,
    pub column: Option<u32>,
}

impl PointLocation {
    pub fn display(&self) -> String {
        match self.column {
            Some(column) => format!("{}:{}:{column}", self.url, self.line),
            None => format!("{}:{}", self.url, self.line),
        }
    }
}

/// Parse `file.js:line[:col]`. The URL may itself contain colons (`http://…`), so numbers are
/// peeled from the right.
pub fn parse_location(text: &str) -> Result<PointLocation, String> {
    let mut segments: Vec<&str> = text.split(':').collect();
    let mut numbers: Vec<u32> = Vec::new();
    while numbers.len() < 2 && segments.len() > 1 {
        let last = segments.last().expect("len > 1");
        let Ok(number) = last.parse::<u32>() else {
            break;
        };
        numbers.push(number);
        segments.pop();
    }
    let url = segments.join(":");
    let (line, column) = match numbers.as_slice() {
        [line] => (*line, None),
        [column, line] => (*line, Some(*column)),
        _ => return Err(format!("invalid location '{text}' — expected file.js:line[:col]")),
    };
    if url.is_empty() {
        return Err(format!("invalid location '{text}' — missing script URL"));
    }
    if line == 0 {
        return Err("line numbers are 1-based".to_owned());
    }
    Ok(PointLocation { url, line, column })
}

/// Default trace name for a logpoint: the URL's last path segment plus the line.
pub fn default_point_name(location: &PointLocation) -> String {
    let file = location.url.rsplit('/').next().unwrap_or(&location.url);
    format!("{file}-{}", location.line)
}

/// How `Debugger.setBreakpointByUrl` should match the location's URL: an absolute URL matches
/// exactly; a bare suffix becomes an anchored regex so `renderer.js` can't match `xrenderer.js`.
/// The suffix tolerates a query string — dev servers serve `/src/cart.ts?t=169…` and rotate the
/// stamp on every HMR edit.
pub fn url_match(location: &PointLocation) -> UrlMatch {
    if location.url.contains("://") {
        UrlMatch::Exact(location.url.clone())
    } else {
        UrlMatch::Regex(format!("(^|/){}($|\\?)", regex_escape(&location.url)))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum UrlMatch {
    Exact(String),
    Regex(String),
}

fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(
            ch,
            '.' | '^'
                | '$'
                | '*'
                | '+'
                | '?'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '|'
                | '\\'
                | '/'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Build a logpoint's breakpoint condition: evaluate in the frame's scope, rate-limit, ship the
/// value over the binding, and **always return false** — V8 never pauses on a falsy condition.
///
/// The user's `expr`/`when` are spliced as code (they must be — they reference frame locals), so
/// the daemon compile-checks them *before* arming: a syntax error here would otherwise make the
/// whole condition uncompilable, which V8 treats as no-break — a trace that is silently dead
/// forever. Compiling is still not running: an expression can *throw* at the site (TDZ, a name
/// that isn't in this frame's scope), so both splices are individually caught and the failure is
/// shipped as the row's value — a runtime error must be readable evidence, never a silent skip.
/// State lives on `globalThis` because conditions have no closure to persist in.
pub fn logpoint_condition(name: &str, expr: Option<&str>, when: Option<&str>, rate: u64) -> String {
    let key = encode(name);
    let rate = rate.clamp(RATE_FLOOR, RATE_CEIL);
    let gate = match when {
        Some(when) => format!(
            r#"var gate; try {{ gate = !!({when}); }} catch (e) {{ err = "(when threw: " + String(e && e.message || e).slice(0, 120) + ")"; }}
    if (gate === false) return false;
    "#
        ),
        None => String::new(),
    };
    let value = match expr {
        Some(expr) => format!(
            r#"var v; try {{ v = ({PREVIEW_JS})(({expr}), 0); }} catch (e) {{ err = err || "(expr threw: " + String(e && e.message || e).slice(0, 120) + ")"; }}"#
        ),
        None => "var v;".to_owned(),
    };
    format!(
        r#"(function () {{
  try {{
    var err = null;
    {gate}var rt = globalThis.__kit_trace_rt__ || (globalThis.__kit_trace_rt__ = {{ traces: Object.create(null) }});
    var st = rt.traces[{key}] || (rt.traces[{key}] = {{ hits: 0, emitted: 0, dropped: 0, emitFails: 0, windowStart: 0 }});
    st.hits++;
    var now = Date.now();
    if (now - st.windowStart >= 1000) {{
      if (st.dropped > 0) {{ try {{ __kit_trace__(JSON.stringify({{ t: {key}, s: st.dropped }})); }} catch (_) {{ st.emitFails++; }} }}
      st.windowStart = now; st.emitted = 0; st.dropped = 0;
    }}
    if (st.emitted >= {rate}) {{ st.dropped++; return false; }}
    st.emitted++;
    {value}
    if (err) v = err;
    try {{ __kit_trace__(JSON.stringify({{ t: {key}, v: v }})); }} catch (_) {{ st.emitFails++; }}
  }} catch (_) {{}}
  return false;
}})()"#
    )
}

/// Drop a trace's in-page rate state, so re-adding the same name starts from clean counters.
pub fn clear_state_js(name: &str) -> String {
    let key = encode(name);
    format!(
        "(() => {{ const rt = globalThis.__kit_trace_rt__; if (rt && rt.traces) delete rt.traces[{key}]; }})()"
    )
}

/// The bounded value serializer, one source for the fn-trace runtime and logpoint conditions.
/// Reads only data properties (a getter is reported, never invoked — observation must not run
/// app code), caps depth and width, and never throws.
const PREVIEW_JS: &str = r#"function preview(v, depth = 0) {
    try {
      if (v === null) return "null";
      const t = typeof v;
      if (t === "undefined") return "undefined";
      if (t === "string") return v.length > 80 ? JSON.stringify(v.slice(0, 79) + "…") : JSON.stringify(v);
      if (t === "number" || t === "boolean") return String(v);
      if (t === "bigint") return String(v) + "n";
      if (t === "function") return "fn " + (v.name || "anonymous");
      if (t === "symbol") return v.toString();
      if (v instanceof Error) return (v.name || "Error") + ": " + String(v.message).slice(0, 120);
      if (Array.isArray(v)) {
        if (depth >= 2) return "[…" + v.length + "]";
        const head = v.slice(0, 5).map((x) => preview(x, depth + 1)).join(", ");
        return "[" + head + (v.length > 5 ? ", …" : "") + "]";
      }
      if (t === "object") {
        if (depth >= 2) return "{…}";
        const keys = Object.keys(v);
        const head = keys.slice(0, 6).map((k) => {
          const d = Object.getOwnPropertyDescriptor(v, k);
          return k + ": " + (d && "value" in d ? preview(d.value, depth + 1) : "(getter)");
        }).join(", ");
        return "{" + head + (keys.length > 6 ? ", …" : "") + "}";
      }
      return String(v);
    } catch (_) {
      return "(unpreviewable)";
    }
  }"#;

/// The shared in-page runtime. Field-patched rather than assigned whole: a logpoint condition
/// may have already created a minimal `__kit_trace_rt__` in this context, and clobbering it
/// would drop live rate counters. Pristine references (`now`, `stringify`) are captured *before*
/// any wrapper exists, so tracing a built-in can never recurse through kit's own bookkeeping;
/// `busy` covers the rest.
fn runtime_js() -> String {
    format!(
        r#"
const rt = globalThis.__kit_trace_rt__ ||= {{}};
rt.traces ||= Object.create(null);
rt.busy ||= false;
rt.now ||= performance.now.bind(performance);
rt.dateNow ||= Date.now.bind(Date);
rt.stringify ||= JSON.stringify.bind(JSON);
rt.preview ||= {PREVIEW_JS};"#
    )
}

/// Build the install script for a fn trace. Evaluated in the page; returns a JSON status object
/// (`{ok: true}` or `{ok: false, error}`) so arming failures read as sentences, not silence.
///
/// The wrapper preserves call semantics: `this`/args/return/throw pass through, `new` goes via
/// `Reflect.construct` with the caller's `new.target`, name/length are mirrored, and previews are
/// built only for hits that will actually emit. Thenable results come back as a *derived* promise
/// (settlement and rejection propagate; identity changes) — the daemon's reply discloses this.
pub fn install_fn_js(name: &str, path_segments: &[String], rate: u64) -> String {
    let key = encode(name);
    let parts = serde_json::to_string(path_segments).expect("string array");
    let rate = rate.clamp(RATE_FLOOR, RATE_CEIL);
    let runtime = runtime_js();
    format!(
        r#"(() => {{
{runtime}
  const key = {key};
  const parts = {parts};
  if (rt.traces[key]) return {{ ok: true, already: true }};
  const ownerName = parts.length > 1 ? parts.slice(0, -1).join(".") : "globalThis";
  let owner = globalThis;
  for (let i = 0; i < parts.length - 1; i++) {{
    owner = owner == null ? owner : owner[parts[i]];
  }}
  if (owner == null || (typeof owner !== "object" && typeof owner !== "function")) {{
    return {{ ok: false, error: "path not reachable: '" + ownerName + "' is " + String(owner) }};
  }}
  const prop = parts[parts.length - 1];
  let holder = owner, desc;
  while (holder && !(desc = Object.getOwnPropertyDescriptor(holder, prop))) {{
    holder = Object.getPrototypeOf(holder);
  }}
  if (!desc) return {{ ok: false, error: "no property '" + prop + "' on " + ownerName }};
  if (desc.get || desc.set) return {{ ok: false, error: "'" + prop + "' is an accessor property — not wrappable" }};
  const original = desc.value;
  if (typeof original !== "function") return {{ ok: false, error: "'" + prop + "' is " + typeof original + ", not a function" }};
  if (!desc.writable && !desc.configurable) return {{ ok: false, error: "'" + prop + "' is read-only — cannot wrap" }};
  const st = {{
    original, wrapper: null, shadowed: holder !== owner,
    hits: 0, emitted: 0, dropped: 0, emitFails: 0, windowStart: 0,
  }};
  const emit = (payload) => {{
    try {{ __kit_trace__(rt.stringify(payload)); }} catch (_) {{ st.emitFails++; }}
  }};
  const record = (t0, outcome, getValue, getPreview) => {{
    if (rt.busy) return;
    rt.busy = true;
    try {{
      st.hits++;
      const now = rt.dateNow();
      if (now - st.windowStart >= 1000) {{
        if (st.dropped > 0) emit({{ t: key, s: st.dropped }});
        st.windowStart = now; st.emitted = 0; st.dropped = 0;
      }}
      if (st.emitted >= {rate}) {{ st.dropped++; return; }}
      st.emitted++;
      emit({{ t: key, v: getValue(), o: {{ k: outcome, p: getPreview() }}, d: rt.now() - t0 }});
    }} catch (_) {{
      st.emitFails++;
    }} finally {{
      rt.busy = false;
    }}
  }};
  const argsPreview = (args) => "(" + args.map((a) => rt.preview(a)).join(", ") + ")";
  const wrapper = function (...args) {{
    if (rt.busy) {{
      return new.target ? Reflect.construct(original, args, new.target) : original.apply(this, args);
    }}
    const t0 = rt.now();
    if (new.target) {{
      try {{
        const instance = Reflect.construct(original, args, new.target);
        record(t0, "r", () => argsPreview(args), () => "[constructed]");
        return instance;
      }} catch (e) {{
        record(t0, "t", () => argsPreview(args), () => rt.preview(e));
        throw e;
      }}
    }}
    try {{
      const result = original.apply(this, args);
      if (result && typeof result.then === "function") {{
        return result.then(
          (v) => {{ record(t0, "r", () => argsPreview(args), () => rt.preview(v)); return v; }},
          (e) => {{ record(t0, "t", () => argsPreview(args), () => rt.preview(e)); throw e; }},
        );
      }}
      record(t0, "r", () => argsPreview(args), () => rt.preview(result));
      return result;
    }} catch (e) {{
      record(t0, "t", () => argsPreview(args), () => rt.preview(e));
      throw e;
    }}
  }};
  try {{
    Object.defineProperty(wrapper, "name", {{ value: original.name, configurable: true }});
    Object.defineProperty(wrapper, "length", {{ value: original.length, configurable: true }});
    wrapper.prototype = original.prototype;
  }} catch (_) {{}}
  st.wrapper = wrapper;
  owner[prop] = wrapper;
  if (owner[prop] !== wrapper) return {{ ok: false, error: "assignment did not stick — frozen or proxied object" }};
  rt.traces[key] = st;
  return {{ ok: true }};
}})()"#
    )
}

/// Build the restore script: put the original back if (and only if) the wrapper is still what's
/// installed — an app that replaced the function since must not have its newer code clobbered.
pub fn restore_fn_js(name: &str, path_segments: &[String]) -> String {
    let key = encode(name);
    let parts = serde_json::to_string(path_segments).expect("string array");
    format!(
        r#"(() => {{
  const rt = globalThis.__kit_trace_rt__;
  const key = {key};
  const st = rt && rt.traces[key];
  if (!st) return {{ status: "missing" }};
  delete rt.traces[key];
  const parts = {parts};
  let owner = globalThis;
  for (let i = 0; i < parts.length - 1; i++) {{
    owner = owner == null ? owner : owner[parts[i]];
  }}
  const prop = parts[parts.length - 1];
  if (owner == null || owner[prop] !== st.wrapper) return {{ status: "replaced" }};
  if (st.shadowed) {{
    delete owner[prop];
  }} else {{
    owner[prop] = st.original;
    if (owner[prop] !== st.original) return {{ status: "blocked" }};
  }}
  return {{ status: "restored" }};
}})()"#
    )
}

/// Build the keeper probe: per-trace liveness plus the suppression counts a silent page would
/// otherwise never flush (the in-page cap only reports drops when the *next* hit arrives).
/// `rt.dateNow` exists only once a fn trace installed the full runtime — a logpoint-only page
/// has the minimal `rt` its conditions create, so the probe must fall back to `Date.now`.
pub fn probe_js(names: &[String]) -> String {
    let keys = serde_json::to_string(names).expect("string array");
    format!(
        r#"(() => {{
  const rt = globalThis.__kit_trace_rt__;
  const out = {{}};
  for (const key of {keys}) {{
    const st = rt && rt.traces && rt.traces[key];
    if (!st) {{ out[key] = {{ installed: false }}; continue; }}
    const now = (rt.dateNow || Date.now)();
    let flush = 0;
    if (st.dropped > 0 && now - st.windowStart >= 1000) {{
      flush = st.dropped;
      st.dropped = 0;
      st.emitted = 0;
      st.windowStart = now;
    }}
    out[key] = {{ installed: true, flush, emitFails: st.emitFails }};
  }}
  return out;
}})()"#
    )
}

/// What the keeper probe reports for one trace.
#[derive(Debug, Deserialize)]
pub struct ProbeStatus {
    pub installed: bool,
    /// Suppressed hits whose window rolled over with no follow-up hit to carry the count.
    #[serde(default)]
    pub flush: u64,
    /// Hits whose binding call threw — payloads that never reached the daemon. Should be zero;
    /// nonzero means the transport itself is broken, which `trace ls` must say out loud.
    #[serde(default, rename = "emitFails")]
    pub emit_fails: u64,
}

/// One decoded `__kit_trace__` payload: a hit, or a suppression count when `suppressed` is set.
#[derive(Debug)]
pub struct Payload {
    pub name: String,
    pub value: Option<String>,
    pub outcome: Option<TraceOutcome>,
    pub duration_ms: Option<f64>,
    pub suppressed: Option<u64>,
}

#[derive(Deserialize)]
struct WirePayload {
    t: String,
    v: Option<String>,
    o: Option<WireOutcome>,
    d: Option<f64>,
    s: Option<u64>,
}

#[derive(Deserialize)]
struct WireOutcome {
    k: String,
    p: String,
}

/// Decode a binding payload. The binding is callable by page code, so this treats input as
/// untrusted: malformed JSON is `None` (never a panic), and every string is re-bounded even
/// though the page-side serializer already bounds it.
pub fn decode_payload(raw: &str) -> Option<Payload> {
    if raw.len() > PAYLOAD_CAP {
        return None;
    }
    let wire: WirePayload = serde_json::from_str(raw).ok()?;
    validate_name(&wire.t).ok()?;
    let outcome = match wire.o {
        Some(WireOutcome { k, p }) => Some(match k.as_str() {
            "r" => TraceOutcome::Returned(bound(&p)),
            "t" => TraceOutcome::Threw(bound(&p)),
            _ => return None,
        }),
        None => None,
    };
    Some(Payload {
        name: wire.t,
        value: wire.v.as_deref().map(bound),
        outcome,
        duration_ms: wire.d.filter(|ms| ms.is_finite() && *ms >= 0.0),
        suppressed: wire.s,
    })
}

fn bound(text: &str) -> String {
    if text.chars().count() <= PREVIEW_CAP {
        return text.to_owned();
    }
    let kept: String = text.chars().take(PREVIEW_CAP - 1).collect();
    format!("{kept}…")
}

fn encode(text: &str) -> String {
    serde_json::to_string(text).expect("string encodes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fn_paths_are_identifier_chains_or_rejected() {
        assert_eq!(parse_fn_path("app.api.save").unwrap().len(), 3);
        assert_eq!(parse_fn_path("saveAll").unwrap(), vec!["saveAll"]);
        assert!(parse_fn_path("app['x'].save").is_err());
        assert!(parse_fn_path("a.b()").is_err());
        assert!(parse_fn_path("a..b").is_err());
        assert!(parse_fn_path("").is_err());
        assert!(parse_fn_path("1bad").is_err());
        // The path is spliced as code — anything that could escape the splice must be refused.
        assert!(parse_fn_path("a;alert(1)").is_err());
        assert!(parse_fn_path("__kit_trace_rt__.traces").is_err());
    }

    #[test]
    fn names_embed_safely_into_the_templates() {
        assert!(validate_name("save").is_ok());
        assert!(validate_name("api.save-2").is_ok());
        assert!(validate_name("a\"b").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name(&"x".repeat(64)).is_err());
        let script = install_fn_js("save", &parse_fn_path("app.api.save").unwrap(), 20);
        assert!(script.contains("\"save\""));
        assert!(script.contains("Reflect.construct"));
        assert!(script.contains("getOwnPropertyDescriptor"));
    }

    #[test]
    fn locations_parse_from_the_right_and_validate() {
        let plain = parse_location("renderer.js:93").unwrap();
        assert_eq!((plain.url.as_str(), plain.line, plain.column), ("renderer.js", 93, None));
        let with_col = parse_location("src/store.ts:84:12").unwrap();
        assert_eq!(with_col.column, Some(12));
        let absolute = parse_location("http://localhost:3000/bundle.js:7").unwrap();
        assert_eq!(absolute.url, "http://localhost:3000/bundle.js");
        assert_eq!(absolute.line, 7);
        assert!(parse_location("renderer.js").is_err());
        assert!(parse_location("renderer.js:0").is_err());
        assert!(parse_location(":93").is_err());
        assert_eq!(default_point_name(&plain), "renderer.js-93");
    }

    #[test]
    fn url_matching_anchors_suffixes_and_passes_absolutes() {
        let suffix = parse_location("renderer.js:9").unwrap();
        assert_eq!(url_match(&suffix), UrlMatch::Regex(r"(^|/)renderer\.js($|\?)".to_owned()));
        let absolute = parse_location("file:///app/x.js:9").unwrap();
        assert_eq!(url_match(&absolute), UrlMatch::Exact("file:///app/x.js".to_owned()));
    }

    #[test]
    fn probe_survives_a_logpoint_only_runtime() {
        // A logpoint-only page never installs `rt.dateNow` — the probe must not assume it.
        let probe = probe_js(&["hot".to_owned()]);
        assert!(probe.contains("(rt.dateNow || Date.now)()"));
        assert!(!probe.contains("rt.dateNow()"));
    }

    #[test]
    fn logpoint_conditions_always_return_false_and_gate_on_when() {
        let bare = logpoint_condition("hot", None, None, 20);
        assert!(bare.ends_with("})()"));
        assert!(bare.contains("return false;\n})()"));
        assert!(!bare.contains("var gate"));
        let gated = logpoint_condition("hot", Some("items.length"), Some("retries > 0"), 20);
        assert!(gated.contains("gate = !!(retries > 0)"));
        assert!(gated.contains("if (gate === false) return false;"));
        assert!(gated.contains("(items.length)"));
        // A splice that compiles can still throw at the site (TDZ, out-of-scope name) — the
        // failure must ship as the row's value, never vanish into a counter.
        assert!(gated.contains("expr threw"));
        assert!(gated.contains("when threw"));
    }

    #[test]
    fn payloads_decode_bounded_and_reject_garbage() {
        let hit = decode_payload(
            r#"{"t":"save","v":"(2 args)","o":{"k":"r","p":"{ok: true}"},"d":38.2}"#,
        )
        .unwrap();
        assert_eq!(hit.name, "save");
        assert_eq!(hit.value.as_deref(), Some("(2 args)"));
        assert!(matches!(hit.outcome, Some(TraceOutcome::Returned(p)) if p == "{ok: true}"));
        assert_eq!(hit.duration_ms, Some(38.2));

        let suppressed = decode_payload(r#"{"t":"hot","s":412}"#).unwrap();
        assert_eq!(suppressed.suppressed, Some(412));
        assert!(suppressed.outcome.is_none());

        assert!(decode_payload("not json").is_none());
        assert!(decode_payload(r#"{"t":"bad name!","s":1}"#).is_none());
        assert!(decode_payload(r#"{"t":"x","o":{"k":"zz","p":""}}"#).is_none());
        assert!(decode_payload(r#"{"t":"x","d":-5.0}"#).unwrap().duration_ms.is_none());
        let huge = format!(r#"{{"t":"x","v":"{}"}}"#, "y".repeat(20_000));
        assert!(decode_payload(&huge).is_none());

        let long = format!(r#"{{"t":"x","v":"{}"}}"#, "y".repeat(500));
        let decoded = decode_payload(&long).unwrap();
        assert_eq!(decoded.value.unwrap().chars().count(), 240);
    }
}
