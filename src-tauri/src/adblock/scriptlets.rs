//! AppMaka-owned scriptlet implementations for the adblock engine.
//!
//! Why this file exists: YouTube and YouTube Music serve video ads from the
//! same domains as the videos, so network rules cannot separate ad segments
//! from content. uBlock Origin kills them with scriptlets — small page
//! scripts referenced by `+js(...)` filter rules. The engine
//! (`adblock-rust`) already knows how to match those rules and assemble the
//! scripts; what it needs is the scriptlet code itself, supplied here as
//! `Resource`s.
//!
//! These implementations are AppMaka's own, written to the generic
//! operations the filter lists reference (`set` a property, prune JSON
//! paths, replace XHR/fetch response text). No uBlock scriptlet code is
//! copied. The filter LISTS themselves (uBlock filters, privacy, unbreak,
//! quick-fixes) are fetched at runtime from uAssets, the same way EasyList
//! and EasyPrivacy already are — they are data, not shipped code.
//!
//! Licensing note: only the `trusted-*` scriptlets get the TRUSTED
//! permission bit, and only the uBlock lists are parsed with that bit, so
//! no other list can invoke them. There is deliberately no
//! `trusted-set-cookie` here: nothing in the YouTube rules needs cookie
//! access, so that power is not offered at all.
//!
//! Each scriptlet is one JS function; the engine extracts its name from the
//! definition and emits a call with the rule's args (all args arrive as
//! strings, quoted). Function bodies must therefore start with
//! `function <name>(` at the very start of the string — no leading comment
//! or blank line — or the engine falls back to `{{1}}` template patching
//! and the args are lost. Shared helpers live in the `am-helpers.js`
//! dependency, emitted verbatim before the calls that use them.

use adblock::resources::{MimeType, PermissionMask, Resource, ResourceType};
use base64::Engine as _;

/// Permission bit granted only to the uBlock lists at parse time, so only
/// they can invoke the `trusted-*` scriptlets below.
pub const TRUSTED: PermissionMask = PermissionMask::from_bits(0b00000001);

/// Shared helpers for the scriptlets below. Emitted verbatim (a dependency,
/// never invoked directly), so it may start with a comment.
const HELPERS_JS: &str = r##"// AppMaka scriptlet helpers (shared; emitted before the calls below).
function amToRegex(s) {
    if (s.length >= 2 && s.charAt(0) === '/') {
        var last = s.lastIndexOf('/');
        if (last > 0) {
            try { return new RegExp(s.slice(1, last), s.slice(last + 1)); } catch (e) { return null; }
        }
    }
    return null;
}
function amMatchUrl(pattern, url) {
    var p = pattern;
    if (p.indexOf('url:') === 0) { p = p.slice(4); }
    var re = amToRegex(p);
    if (re !== null) { try { return re.test(url); } catch (e) { return false; } }
    return url.indexOf(p) !== -1;
}
function amFetchUrl(input) {
    if (typeof input === 'string') { return input; }
    if (input && typeof input.url === 'string') { return input.url; }
    return '';
}
function amSplitPaths(s) {
    return String(s).split(/\s+/).filter(function(p) { return p !== '' && p !== 'important'; });
}
function amPrunePaths(root, paths) {
    function prune(obj, parts, idx) {
        if (obj === null || typeof obj !== 'object') { return; }
        if (idx >= parts.length) { return; }
        var seg = parts[idx];
        if (seg === '[-]') {
            if (Array.isArray(obj)) {
                for (var i = 0; i < obj.length; i++) { prune(obj[i], parts, idx + 1); }
            }
            return;
        }
        if (idx === parts.length - 1) {
            try { delete obj[seg]; } catch (e) {}
            return;
        }
        var next = null;
        try { next = obj[seg]; } catch (e) {}
        if (next !== null && typeof next === 'object') { prune(next, parts, idx + 1); }
    }
    for (var i = 0; i < paths.length; i++) {
        var parts = paths[i].split('.');
        if (parts.length > 0 && parts[0] !== '') { prune(root, parts, 0); }
    }
}
function amLiteralFallback(s) {
    if (s.length >= 2 && s.charAt(0) === '/') {
        var last = s.lastIndexOf('/');
        if (last > 0) { return s.slice(1, last); }
    }
    return s;
}
function amParseEditPattern(pattern) {
    var p = String(pattern);
    var recursive = false;
    if (p.indexOf('..') === 0) { recursive = true; p = p.slice(2); }
    else if (p.charAt(0) === '.') { p = p.slice(1); }
    var raw = [];
    var cur = '';
    var inBracket = false;
    for (var i = 0; i < p.length; i++) {
        var ch = p.charAt(i);
        if (ch === '[' && !inBracket) {
            if (cur !== '') { raw.push(cur); cur = ''; }
            inBracket = true;
            cur = '[';
        } else if (ch === ']' && inBracket) {
            cur += ']';
            raw.push(cur); cur = '';
            inBracket = false;
        } else if (ch === '.' && !inBracket) {
            if (cur !== '') { raw.push(cur); cur = ''; }
        } else { cur += ch; }
    }
    if (cur !== '') { raw.push(cur); }
    var out = [];
    for (var j = 0; j < raw.length; j++) {
        var s = raw[j];
        if (s === '*') { out.push({ star: true }); }
        else if (s.indexOf('[?') === 0 && s.charAt(s.length - 1) === ']') {
            var key = s.slice(2, -1);
            if (key.charAt(0) === '.') { key = key.slice(1); }
            out.push({ hasKey: key });
        } else { out.push({ key: s }); }
    }
    return { recursive: recursive, segs: out };
}
function amHasKeyDeep(obj, key, depth) {
    if (obj === null || typeof obj !== 'object' || depth > 32) { return false; }
    if (Object.prototype.hasOwnProperty.call(obj, key)) { return true; }
    for (var k in obj) {
        if (Object.prototype.hasOwnProperty.call(obj, k)) {
            if (amHasKeyDeep(obj[k], key, depth + 1)) { return true; }
        }
    }
    return false;
}
function amEditApply(node, segs, idx) {
    if (node === null || typeof node !== 'object') { return; }
    if (idx >= segs.length) { return; }
    var seg = segs[idx];
    if (seg.star) {
        if (Array.isArray(node)) {
            var next = segs[idx + 1];
            if (next && next.hasKey) {
                for (var i = node.length - 1; i >= 0; i--) {
                    if (amHasKeyDeep(node[i], next.hasKey, 0)) { node.splice(i, 1); }
                }
            } else {
                for (var j = 0; j < node.length; j++) { amEditApply(node[j], segs, idx + 1); }
            }
        }
        return;
    }
    if (seg.hasKey) { return; }
    var child = node[seg.key];
    if (child === undefined) { return; }
    if (idx === segs.length - 1) { return; }
    amEditApply(child, segs, idx + 1);
}
function amEditWalk(node, parsed) {
    if (parsed.recursive) {
        amEditApply(node, parsed.segs, 0);
        if (node !== null && typeof node === 'object') {
            for (var k in node) {
                if (Object.prototype.hasOwnProperty.call(node, k)) {
                    amEditWalk(node[k], parsed);
                }
            }
        }
        return;
    }
    amEditApply(node, parsed.segs, 0);
}
"##;

// `set`: pin a dotted window path to a value, and keep it pinned when the
// page reassigns an intermediate object (the YouTube player data is
// assigned after this script runs, so a plain assignment would not stick).
const SET_JS: &str = r##"function set(path, valueStr) {
    var value;
    if (valueStr === 'undefined') { value = undefined; }
    else if (valueStr === 'true') { value = true; }
    else if (valueStr === 'false') { value = false; }
    else if (valueStr === 'null') { value = null; }
    else if (valueStr === '') { value = ''; }
    else if (/^-?\d+(\.\d+)?$/.test(valueStr)) { value = Number(valueStr); }
    else if (valueStr.charAt(0) === '[' || valueStr.charAt(0) === '{') {
        try { value = JSON.parse(valueStr); } catch (e) { value = valueStr; }
    } else { value = valueStr; }
    function trap(owner, parts, idx) {
        if (idx >= parts.length) { return; }
        var seg = parts[idx];
        if (idx === parts.length - 1) {
            try {
                Object.defineProperty(owner, seg, {
                    configurable: true, enumerable: true,
                    get: function() { return value; },
                    set: function() {}
                });
            } catch (e) {}
            return;
        }
        var cur = null;
        try { cur = owner[seg]; } catch (e) {}
        if (cur === null || (typeof cur !== 'object' && typeof cur !== 'function')) {
            cur = {};
            try { owner[seg] = cur; } catch (e) { return; }
        }
        trap(cur, parts, idx + 1);
        // Chain onto a previous set-rule's accessor for this same property
        // instead of replacing it, so several rules sharing an intermediate
        // (ytInitialPlayerResponse.playerAds + .adPlacements) all re-trap
        // when the page assigns a new object.
        var prevSet = null;
        try {
            var desc = Object.getOwnPropertyDescriptor(owner, seg);
            if (desc && typeof desc.set === 'function' && desc.set.__amSetTrap) { prevSet = desc.set; }
        } catch (e) {}
        var held = cur;
        function myGet() { return held; }
        function mySet(v) {
            if (prevSet) { try { prevSet(v); } catch (e) {} }
            held = v;
            if (v !== null && (typeof v === 'object' || typeof v === 'function')) {
                trap(v, parts, idx + 1);
            }
        }
        myGet.__amSetTrap = true;
        mySet.__amSetTrap = true;
        try {
            Object.defineProperty(owner, seg, {
                configurable: true, enumerable: true,
                get: myGet, set: mySet
            });
        } catch (e) {}
    }
    var parts = String(path).split('.');
    if (parts.length > 0 && parts[0] !== '') { trap(window, parts, 0); }
}"##;

// `json-prune`: wrap JSON.parse so the listed paths are deleted from every
// parsed object. `[-]` walks every array element; `important` is accepted
// and ignored (pruning always applies here).
const JSON_PRUNE_JS: &str = r##"function jsonPrune(pathsStr) {
    var paths = amSplitPaths(pathsStr);
    var origParse = JSON.parse;
    JSON.parse = function(text, reviver) {
        var result = origParse.call(this, text, reviver);
        try { amPrunePaths(result, paths); } catch (e) {}
        return result;
    };
}"##;

// `json-prune-fetch-response`: prune paths from JSON fetch responses whose
// URL matches. The second arg is a match-condition list (empty in the
// YouTube rules); when given, pruning only applies if the JSON contains
// one of those paths.
const JSON_PRUNE_FETCH_RESPONSE_JS: &str = r##"function jsonPruneFetchResponse(pathsStr, matchStr, mode, urlPattern) {
    var paths = amSplitPaths(pathsStr);
    var matchPaths = amSplitPaths(matchStr || '');
    var origFetch = window.fetch;
    if (typeof origFetch !== 'function') { return; }
    window.fetch = function(input, init) {
        var url = amFetchUrl(input);
        var should = (typeof urlPattern === 'string' && urlPattern !== '') ? amMatchUrl(urlPattern, url) : true;
        return origFetch.apply(this, arguments).then(function(resp) {
            if (!should) { return resp; }
            var ct = '';
            try { ct = resp.headers.get('content-type') || ''; } catch (e) {}
            if (ct.indexOf('json') === -1 && ct !== '') { return resp; }
            return resp.clone().text().then(function(text) {
                var data;
                try { data = JSON.parse(text); } catch (e) { return resp; }
                if (matchPaths.length > 0) {
                    var found = false;
                    for (var i = 0; i < matchPaths.length; i++) {
                        var first = matchPaths[i].split('.')[0];
                        if (first !== '' && first !== '[-]' && data !== null && typeof data === 'object' && first in data) { found = true; break; }
                    }
                    if (!found) { return resp; }
                }
                try { amPrunePaths(data, paths); } catch (e) {}
                return new Response(JSON.stringify(data), { status: resp.status, statusText: resp.statusText, headers: resp.headers });
            }).catch(function() { return resp; });
        });
    };
}"##;

// `json-edit-fetch-response`: the `..a.b.*[?.KEY]` pattern shape —
// recursively find `a.b` arrays and drop elements containing KEY. This
// covers the YouTube watch-page rule; the full JSONPath query language
// is out of scope.
const JSON_EDIT_FETCH_RESPONSE_JS: &str = r##"function jsonEditFetchResponse(pattern, mode, urlPattern) {
    var parsed = amParseEditPattern(pattern);
    var origFetch = window.fetch;
    if (typeof origFetch !== 'function' || parsed === null) { return; }
    window.fetch = function(input, init) {
        var url = amFetchUrl(input);
        var should = (typeof urlPattern === 'string' && urlPattern !== '') ? amMatchUrl(urlPattern, url) : true;
        return origFetch.apply(this, arguments).then(function(resp) {
            if (!should) { return resp; }
            return resp.clone().text().then(function(text) {
                var data;
                try { data = JSON.parse(text); } catch (e) { return resp; }
                try { amEditWalk(data, parsed); } catch (e) {}
                return new Response(JSON.stringify(data), { status: resp.status, statusText: resp.statusText, headers: resp.headers });
            }).catch(function() { return resp; });
        });
    };
}"##;

// `trusted-replace-xhr-response`: regex-replace XHR response text for
// matching URLs. Patched once on the prototype; every rule appends to a
// shared rule list so several rules can apply to the same page. Only the
// uBlock lists may invoke this (TRUSTED), and it never touches cookies.
const TRUSTED_REPLACE_XHR_RESPONSE_JS: &str = r##"function trustedReplaceXhrResponse(pattern, replacement, urlPattern) {
    var re = amToRegex(pattern);
    function applyOne(text, pat, rep) {
        if (pat !== null) { try { return text.replace(pat, rep); } catch (e) { return text; } }
        return text.split(amLiteralFallback(pattern)).join(rep);
    }
    var proto = XMLHttpRequest.prototype;
    if (!proto) { return; }
    var rules = proto.__amXhrRules;
    if (!rules) {
        rules = [];
        try { Object.defineProperty(proto, '__amXhrRules', { value: rules }); } catch (e) { return; }
        var origOpen = proto.open;
        var textDesc = null;
        var respDesc = null;
        try { textDesc = Object.getOwnPropertyDescriptor(proto, 'responseText'); } catch (e) {}
        try { respDesc = Object.getOwnPropertyDescriptor(proto, 'response'); } catch (e) {}
        proto.open = function(method, url) {
            try { this.__amXhrUrl = url; } catch (e) {}
            return origOpen.apply(this, arguments);
        };
        function matchingRules(xhr) {
            var url = '';
            try { url = xhr.__amXhrUrl || ''; } catch (e) {}
            var out = [];
            for (var i = 0; i < rules.length; i++) {
                var r = rules[i];
                if (typeof r.urlPattern !== 'string' || r.urlPattern === '' || amMatchUrl(r.urlPattern, url)) { out.push(r); }
            }
            return out;
        }
        if (textDesc && typeof textDesc.get === 'function') {
            try {
                Object.defineProperty(proto, 'responseText', {
                    configurable: true, enumerable: textDesc.enumerable,
                    get: function() {
                        var t = textDesc.get.call(this);
                        if (typeof t !== 'string') { return t; }
                        var rs = matchingRules(this);
                        for (var i = 0; i < rs.length; i++) { t = applyOne(t, rs[i].re, rs[i].replacement); }
                        return t;
                    }
                });
            } catch (e) {}
        }
        if (respDesc && typeof respDesc.get === 'function') {
            try {
                Object.defineProperty(proto, 'response', {
                    configurable: true, enumerable: respDesc.enumerable,
                    get: function() {
                        var v = respDesc.get.call(this);
                        var rs = matchingRules(this);
                        if (rs.length === 0) { return v; }
                        var rt = this.responseType;
                        if ((rt === '' || rt === 'text') && typeof v === 'string') {
                            for (var i = 0; i < rs.length; i++) { v = applyOne(v, rs[i].re, rs[i].replacement); }
                            return v;
                        }
                        if (rt === 'json') {
                            try {
                                var t = textDesc ? textDesc.get.call(this) : null;
                                if (typeof t === 'string') {
                                    for (var j = 0; j < rs.length; j++) { t = applyOne(t, rs[j].re, rs[j].replacement); }
                                    return JSON.parse(t);
                                }
                            } catch (e) {}
                        }
                        return v;
                    }
                });
            } catch (e) {}
        }
    }
    rules.push({ re: re, replacement: replacement, urlPattern: urlPattern });
}"##;

// `trusted-rpfr`: the fetch twin of the XHR replacer — literal or regex
// text replacement on fetch responses whose URL matches. The YouTube
// player rules use literal quoted strings ('"adPlacements"' -> '"no_ads"')
// with a 'player?' URL substring.
const TRUSTED_RPFR_JS: &str = r##"function trustedRpfr(pattern, replacement, urlPattern) {
    var re = amToRegex(pattern);
    function apply(text) {
        if (re !== null) { try { return text.replace(re, replacement); } catch (e) { return text; } }
        return text.split(amLiteralFallback(pattern)).join(replacement);
    }
    var origFetch = window.fetch;
    if (typeof origFetch !== 'function') { return; }
    window.fetch = function(input, init) {
        var url = amFetchUrl(input);
        var should = (typeof urlPattern === 'string' && urlPattern !== '') ? amMatchUrl(urlPattern, url) : true;
        return origFetch.apply(this, arguments).then(function(resp) {
            if (!should) { return resp; }
            return resp.clone().text().then(function(text) {
                return new Response(apply(text), { status: resp.status, statusText: resp.statusText, headers: resp.headers });
            }).catch(function() { return resp; });
        });
    };
}"##;

// `nowoif` (no-window-open-if) with no args: block every window.open.
// The arg-conditional form is not implemented; no current rule needs it.
const NOWOIF_JS: &str = r##"function nowoif() {
    try { window.open = function() { return null; }; } catch (e) {}
}"##;

fn resource(
    name: &str,
    js: &str,
    dependencies: Vec<String>,
    permission: PermissionMask,
) -> Resource {
    Resource {
        name: name.to_string(),
        aliases: vec![],
        kind: ResourceType::Mime(MimeType::ApplicationJavascript),
        content: base64::engine::general_purpose::STANDARD.encode(js),
        dependencies,
        permission,
    }
}

/// The full scriptlet set handed to `Engine::use_resources`. The helper
/// bundle is a dependency, never invoked by a filter directly.
pub fn resources() -> Vec<Resource> {
    let dep = "am-helpers.js".to_string();
    vec![
        resource("am-helpers.js", HELPERS_JS, vec![], PermissionMask::default()),
        resource("set.js", SET_JS, vec![], PermissionMask::default()),
        resource(
            "json-prune.js",
            JSON_PRUNE_JS,
            vec![dep.clone()],
            PermissionMask::default(),
        ),
        resource(
            "json-prune-fetch-response.js",
            JSON_PRUNE_FETCH_RESPONSE_JS,
            vec![dep.clone()],
            PermissionMask::default(),
        ),
        resource(
            "json-edit-fetch-response.js",
            JSON_EDIT_FETCH_RESPONSE_JS,
            vec![dep.clone()],
            PermissionMask::default(),
        ),
        resource(
            "trusted-replace-xhr-response.js",
            TRUSTED_REPLACE_XHR_RESPONSE_JS,
            vec![dep.clone()],
            TRUSTED,
        ),
        resource(
            "trusted-rpfr.js",
            TRUSTED_RPFR_JS,
            vec![dep.clone()],
            TRUSTED,
        ),
        resource("nowoif.js", NOWOIF_JS, vec![], PermissionMask::default()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use adblock::engine::Engine;
    use adblock::lists::{FilterSet, ParseOptions};

    /// Real YouTube rule lines from uBlock filters (shape-exact, trimmed to
    /// the scriptlets this build implements).
    const YT_RULES: &str = r##"m.youtube.com,music.youtube.com,tv.youtube.com,www.youtube.com,youtubekids.com,youtube-nocookie.com##+js(set, ytInitialPlayerResponse.playerAds, undefined)
m.youtube.com,music.youtube.com,tv.youtube.com,www.youtube.com,youtubekids.com,youtube-nocookie.com##+js(set, ytInitialPlayerResponse.adPlacements, undefined)
m.youtube.com,music.youtube.com,tv.youtube.com,www.youtube.com,youtubekids.com,youtube-nocookie.com##+js(json-prune, playerResponse.adPlacements playerResponse.playerAds playerResponse.adSlots adPlacements playerAds adSlots important)
www.youtube.com##+js(trusted-rpfr, '"adPlacements"', '"no_ads"', player?)
www.youtube.com##+js(trusted-replace-xhr-response, /"adPlacements.*?([A-Z]"\}|\}"\{2\,4})\}\]\,/, , /playlist\?list=|\/player(?:\?.+)?$|watch\?[tv]=/)"##;

    fn engine_with(rules: &str, trusted: bool) -> Engine {
        let mut set = FilterSet::new(false);
        let opts = if trusted {
            ParseOptions {
                permissions: TRUSTED,
                ..Default::default()
            }
        } else {
            ParseOptions::default()
        };
        set.add_filter_list(rules.to_string(), opts);
        let mut engine = Engine::new_with_filter_set(set);
        engine.use_resources(resources());
        engine
    }

    #[test]
    fn resource_names_cover_the_youtube_set() {
        let all = resources();
        let names: Vec<&str> = all.iter().map(|r| r.name.as_str()).collect();
        for want in [
            "am-helpers.js",
            "set.js",
            "json-prune.js",
            "json-prune-fetch-response.js",
            "json-edit-fetch-response.js",
            "trusted-replace-xhr-response.js",
            "trusted-rpfr.js",
            "nowoif.js",
        ] {
            assert!(names.contains(&want), "missing resource {want}");
        }
    }

    #[test]
    fn youtube_url_gets_scriptlets_and_other_urls_do_not() {
        let engine = engine_with(YT_RULES, true);
        let yt = engine.url_cosmetic_resources("https://www.youtube.com/watch?v=x");
        assert!(
            !yt.injected_script.is_empty(),
            "expected scriptlets for youtube.com"
        );
        // The `set` invocation carries the rule's path and value, quoted.
        assert!(
            yt.injected_script
                .contains("set(\"ytInitialPlayerResponse.playerAds\", \"undefined\")"),
            "set invocation missing or mis-quoted: {}",
            yt.injected_script
        );
        // Function definitions ride along as dependencies, including the
        // shared helper bundle (proves dependency emission works).
        assert!(
            yt.injected_script.contains("function jsonPrune("),
            "json-prune definition missing"
        );
        assert!(
            yt.injected_script.contains("function amPrunePaths("),
            "shared helper dependency missing"
        );
        let other = engine.url_cosmetic_resources("https://example.com/");
        assert!(
            other.injected_script.is_empty(),
            "unexpected scriptlets for example.com: {}",
            other.injected_script
        );
    }

    #[test]
    fn trusted_scriptlets_need_the_trusted_bit() {
        let untrusted_engine = engine_with(YT_RULES, false);
        let yt = untrusted_engine.url_cosmetic_resources("https://www.youtube.com/watch?v=x");
        // Untrusted rules still apply...
        assert!(
            yt.injected_script.contains("ytInitialPlayerResponse"),
            "untrusted set rule should still apply"
        );
        // ...but the trusted-* scriptlets must not.
        assert!(
            !yt.injected_script.contains("trustedReplaceXhrResponse"),
            "trusted XHR replacer leaked without permission"
        );
        assert!(
            !yt.injected_script.contains("trustedRpfr"),
            "trusted fetch replacer leaked without permission"
        );
        let trusted_engine = engine_with(YT_RULES, true);
        let yt = trusted_engine.url_cosmetic_resources("https://www.youtube.com/watch?v=x");
        assert!(
            yt.injected_script.contains("trustedReplaceXhrResponse"),
            "trusted XHR replacer missing with permission"
        );
        assert!(
            yt.injected_script.contains("trustedRpfr"),
            "trusted fetch replacer missing with permission"
        );
    }
}
