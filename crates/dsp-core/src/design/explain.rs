//! Plain words for a design that does not build at some of the rates it
//! would play, and the edits that make it build at all of them.
//!
//! The engine's refusals ([`DesignError`]) name the limit a design hits in
//! its own terms ("an equiripple design of 1279 taps over this band at x4
//! would be about 237 dB deep, past the 160 dB f64 levels its ripple
//! to"). [`explain`] says the same in the user's: whether the length is
//! too short or too long at each rate (found by trying other lengths with
//! [`super::feasibility`], so every path's refusals are covered without
//! reading their messages), the rates grouped by reason, and what to do.
//! The engine's message stays as each rate's detail.
//!
//! The fixes ([`Fix`]) are whole designs, each checked at every rate given:
//! an automatic length, a fixed number of taps, taps per input sample, or a
//! wider transition (a lower passband end), whichever builds everywhere.

use super::budget::{self, Limit};
use super::{
    BuildContext, Cutoff, DesignError, FilterDesign, FilterErrorBudget, KernelDesign, Length,
    OptimalMethod, Shape, Stepped,
};
use audivane_protocol::RateRatio;

/// One rate the design would play at: its context, and the refusal there
/// (`None` where it builds).
#[derive(Clone, Debug)]
pub struct Case {
    pub cx: BuildContext,
    pub error: Option<DesignError>,
}

/// What [`explain`] says.
#[derive(Clone, Debug, PartialEq)]
pub struct Explanation {
    /// Plain sentences: what is wrong, by group of rates, and what to do.
    pub message: String,
    /// One per case that does not build, in the cases' order.
    pub rates: Vec<RateReason>,
    /// Edits that build at every case, in the order to offer them.
    pub fixes: Vec<Fix>,
}

/// Why one rate does not build.
#[derive(Clone, Debug, PartialEq)]
pub struct RateReason {
    pub source_rate: u32,
    pub ratio: RateRatio,
    /// The design field it is about ([`super::check::refusal`]).
    pub path: String,
    /// A few plain words for this rate alone: `too short here (needs about
    /// 1,600 taps)`.
    pub short: String,
    /// The engine's own message.
    pub detail: String,
}

/// A one-click fix: its label and the design with the edit made.
#[derive(Clone, Debug, PartialEq)]
pub struct Fix {
    pub label: String,
    pub design: FilterDesign,
}

/// What is wrong at one rate, in the user's terms.
#[derive(Clone, Debug, PartialEq)]
enum Problem {
    /// Builds with a longer length, from `need`.
    Short { need: usize },
    /// Builds with a shorter length; `why` says what the length runs into.
    Long { why: &'static str },
    /// The design's rate rules do not take this ratio.
    Ratio,
    /// Over a design limit no length fixes.
    Limit { why: &'static str },
    /// Anything else (the detail says).
    Other,
}

impl Problem {
    /// The group key: the same words for the same reason.
    fn key(&self) -> String {
        match self {
            Problem::Short { .. } => "short".into(),
            Problem::Long { why } => format!("long {why}"),
            Problem::Ratio => "ratio".into(),
            Problem::Limit { why } => format!("limit {why}"),
            Problem::Other => "other".into(),
        }
    }
}

/// The length knob of a design: what [`with_length`] changes.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Knob {
    /// [`Length::Fixed`] taps.
    Fixed(usize),
    /// [`Length::PerInput`], one value at every rate.
    PerInput(usize),
    /// Another length rule (stepped, span, band, automatic).
    Rule,
}

fn kernel(d: &FilterDesign) -> Option<&KernelDesign> {
    match d {
        FilterDesign::Kernel(k) => Some(k),
        _ => None,
    }
}

fn length(d: &FilterDesign) -> Option<&Length> {
    match &kernel(d)?.shape {
        Shape::WindowedSinc { length, .. } | Shape::Optimal { length, .. } => Some(length),
        Shape::Lanczos { .. } => None,
    }
}

fn knob(d: &FilterDesign) -> Option<Knob> {
    Some(match length(d)? {
        Length::Fixed(n) => Knob::Fixed(*n),
        Length::PerInput(Stepped::Fixed(t)) => Knob::PerInput(*t),
        _ => Knob::Rule,
    })
}

/// `d` with its length rule replaced (a kernel with a length rule only).
fn with_length(d: &FilterDesign, l: Length) -> FilterDesign {
    let mut d = d.clone();
    if let FilterDesign::Kernel(k) = &mut d
        && let Shape::WindowedSinc { length, .. } | Shape::Optimal { length, .. } = &mut k.shape
    {
        *length = l;
    }
    d
}

fn optimal_method(d: &FilterDesign) -> Option<OptimalMethod> {
    match &kernel(d)?.shape {
        Shape::Optimal { method, .. } => Some(*method),
        _ => None,
    }
}

fn builds(d: &FilterDesign, cx: &BuildContext) -> bool {
    super::feasibility(d, cx).is_ok()
}

/// The lengths `make(v)` builds with in `cx`, `min..=max`, as one interval
/// around the first that builds from `start` (trying twice and half as
/// long in turn): `None` when none does.
fn interval(
    cx: &BuildContext,
    make: &dyn Fn(usize) -> FilterDesign,
    start: usize,
    min: usize,
    max: usize,
) -> Option<(usize, usize)> {
    let ok = |v: usize| builds(&make(v), cx);
    let s = start.clamp(min, max);
    let anchor = if ok(s) {
        s
    } else {
        let mut found = None;
        let mut v = s;
        while v < max {
            v = v.saturating_mul(2).min(max);
            if ok(v) {
                found = Some(v);
                break;
            }
        }
        if found.is_none() {
            v = s;
            while v > min {
                v = (v / 2).max(min);
                if ok(v) {
                    found = Some(v);
                    break;
                }
            }
        }
        found?
    };
    // The shortest that builds, then the longest (each side monotone).
    let lo = if ok(min) {
        min
    } else {
        let (mut bad, mut good) = (min, anchor);
        while good - bad > 1 {
            let mid = bad + (good - bad) / 2;
            if ok(mid) { good = mid } else { bad = mid }
        }
        good
    };
    let hi = if ok(max) {
        max
    } else {
        let (mut good, mut bad) = (anchor, max);
        while bad - good > 1 {
            let mid = good + (bad - good) / 2;
            if ok(mid) { good = mid } else { bad = mid }
        }
        good
    };
    Some((lo, hi))
}

/// The longest fixed length tried, taps.
const MAX_FIXED: usize = budget::MAX_KERNEL_TAPS;
/// The longest per-input length tried, taps per input sample.
const MAX_PER_INPUT: usize = 65_535;

fn fixed_maker(d: &FilterDesign) -> impl Fn(usize) -> FilterDesign + '_ {
    move |v| with_length(d, Length::Fixed(v))
}

fn per_input_maker(d: &FilterDesign) -> impl Fn(usize) -> FilterDesign + '_ {
    move |v| with_length(d, Length::PerInput(Stepped::Fixed(v)))
}

/// What the length runs into when it is too long, from the refusal at the
/// design's own length.
fn long_why(d: &FilterDesign, e: &DesignError) -> &'static str {
    match e {
        DesignError::TooLong { .. }
        | DesignError::Budget(budget::Budget {
            limit: Limit::KernelTaps,
            ..
        }) => "over the engine's longest kernel",
        DesignError::Budget(budget::Budget {
            limit: Limit::RationalTaps | Limit::RationalCost,
            ..
        }) => "too heavy for a fractional ratio",
        DesignError::Budget(budget::Budget {
            limit: Limit::Workspace,
            ..
        }) => "needs more design memory than this machine allows",
        DesignError::Budget(budget::Budget {
            limit: Limit::CepstralFft,
            ..
        }) => "too long for the minimum-phase transform",
        _ => match optimal_method(d) {
            Some(OptimalMethod::Equiripple) => "too long for an equiripple design",
            Some(OptimalMethod::LeastSquares) => "too long for a least-squares design",
            None => "too long",
        },
    }
}

fn limit_why(e: &DesignError) -> Option<&'static str> {
    match e {
        DesignError::TooLong { .. } => Some("over the engine's longest kernel"),
        DesignError::Budget(b) => Some(match b.limit {
            Limit::KernelTaps => "over the engine's longest kernel",
            Limit::RationalTaps | Limit::RationalCost => "too heavy for a fractional ratio",
            Limit::Workspace => "needs more design memory than this machine allows",
            Limit::CepstralFft => "too long for the minimum-phase transform",
        }),
        _ => None,
    }
}

/// The problem at one case that does not build.
fn classify(d: &FilterDesign, cx: &BuildContext, e: &DesignError) -> Problem {
    if matches!(e, DesignError::Ratio { .. }) {
        return Problem::Ratio;
    }
    let probe = match knob(d) {
        Some(Knob::Fixed(n)) => interval(cx, &fixed_maker(d), n, 3, MAX_FIXED).map(|r| (n, r)),
        Some(Knob::PerInput(t)) => {
            interval(cx, &per_input_maker(d), t, 1, MAX_PER_INPUT).map(|r| (t, r))
        }
        _ => None,
    };
    match probe {
        Some((cur, (lo, _))) if cur < lo => Problem::Short { need: lo },
        Some((cur, (_, hi))) if cur > hi => Problem::Long {
            why: long_why(d, e),
        },
        _ => match limit_why(e) {
            Some(why) => Problem::Limit { why },
            None => Problem::Other,
        },
    }
}

/// "1,279".
pub fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `n` rounded up to two significant digits (exact below 100): what
/// "needs about" says.
fn about(n: usize) -> usize {
    if n < 100 {
        return n;
    }
    let mut step = 1;
    while n / step >= 100 {
        step *= 10;
    }
    n.div_ceil(step) * step
}

/// "44.1k", "48k", "352.8k".
pub fn rate_label(rate: u32) -> String {
    let k = f64::from(rate) / 1000.0;
    if (k - k.round()).abs() < 1e-9 {
        format!("{k:.0}k")
    } else {
        format!("{}k", format!("{k:.3}").trim_end_matches('0'))
    }
}

/// "×16", "×160/147".
pub fn ratio_label(r: RateRatio) -> String {
    if r.down == 1 {
        format!("×{}", r.up)
    } else {
        format!("×{}/{}", r.up, r.down)
    }
}

/// "44.1k ×16".
fn at_label(cx: &BuildContext) -> String {
    format!("{} {}", rate_label(cx.source_rate), ratio_label(cx.ratio))
}

/// "a", "a and b", "a, b and c".
fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [a] => a.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// "a", "a or b", "a, b or c".
fn join_or(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [a] => a.clone(),
        [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
    }
}

/// What the length is, as the sentence's subject.
fn subject(d: &FilterDesign) -> String {
    match knob(d) {
        Some(Knob::Fixed(n)) => format!("{} taps", thousands(n)),
        Some(Knob::PerInput(t)) => format!("{} taps per input sample", thousands(t)),
        _ => "This design".into(),
    }
}

fn unit(d: &FilterDesign) -> &'static str {
    match knob(d) {
        Some(Knob::PerInput(_)) => " per input sample",
        _ => " taps",
    }
}

/// The words for a problem at one rate (`here`) or at a group of rates.
fn phrase(p: &Problem, at: &str, needs: &[usize], detail: &str) -> String {
    match p {
        Problem::Short { .. } => {
            let mut needs: Vec<usize> = needs.iter().map(|n| about(*n)).collect();
            needs.sort_unstable();
            needs.dedup();
            let needs: Vec<String> = needs.into_iter().map(thousands).collect();
            format!("too short {at} (needs about {})", join_and(&needs))
        }
        Problem::Long { why } => format!("{why} {at}"),
        Problem::Ratio => {
            let need = detail
                .split(": it needs ")
                .nth(1)
                .unwrap_or("another ratio");
            format!("not made for the ratio {at} (it needs {need})")
        }
        Problem::Limit { why } => format!("{why} {at}"),
        Problem::Other => format!("not buildable {at}"),
    }
}

/// Plain words for `d` at `cases`, of which at least one does not build;
/// `applied`: the design plays at the rates where it builds (the others
/// play Recommended), so the advice may be to allow fewer rates.
pub fn explain(d: &FilterDesign, cases: &[Case], applied: bool) -> Explanation {
    explain_with(d, cases, applied, &builds)
}

/// [`explain`], each fix offered only when `verify` says it builds at every
/// case: the caller's own acceptance check (the daemon's searches the
/// automatic length and applies the rational budget), so a fix it offers is
/// never refused when applied. The probes that size the fixes stay
/// [`super::feasibility`].
pub fn explain_with(
    d: &FilterDesign,
    cases: &[Case],
    applied: bool,
    verify: &dyn Fn(&FilterDesign, &BuildContext) -> bool,
) -> Explanation {
    let failing: Vec<(usize, &DesignError, Problem)> = cases
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let e = c.error.as_ref()?;
            Some((i, e, classify(d, &c.cx, e)))
        })
        .collect();
    let rates: Vec<RateReason> = failing
        .iter()
        .map(|(i, e, p)| {
            let cx = &cases[*i].cx;
            let needs = match p {
                Problem::Short { need } => vec![*need],
                _ => vec![],
            };
            let mut short = phrase(p, "here", &needs, &e.to_string());
            if matches!(p, Problem::Short { .. }) {
                short = short.replace(')', &format!("{})", unit(d)));
            }
            RateReason {
                source_rate: cx.source_rate,
                ratio: cx.ratio,
                path: super::check::refusal(e, d).path,
                short,
                detail: e.to_string(),
            }
        })
        .collect();
    // Groups in order of first appearance.
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (j, (_, _, p)) in failing.iter().enumerate() {
        match groups.iter_mut().find(|g| g.0 == p.key()) {
            Some(g) => g.1.push(j),
            None => groups.push((p.key(), vec![j])),
        }
    }
    let parts: Vec<String> = groups
        .iter()
        .map(|(_, members)| {
            let (_, e, p) = &failing[members[0]];
            let ats: Vec<String> = members
                .iter()
                .map(|&j| at_label(&cases[failing[j].0].cx))
                .collect();
            let needs: Vec<usize> = members
                .iter()
                .filter_map(|&j| match failing[j].2 {
                    Problem::Short { need } => Some(need),
                    _ => None,
                })
                .collect();
            phrase(p, &format!("at {}", join_and(&ats)), &needs, &e.to_string())
        })
        .collect();
    let fixes = fixes(
        d,
        cases,
        &failing.iter().map(|f| f.2.clone()).collect::<Vec<_>>(),
        verify,
    );
    let mut todo: Vec<String> = fixes.iter().map(|f| lower_first(&f.label)).collect();
    if todo.is_empty() {
        let short = failing.iter().any(|f| matches!(f.2, Problem::Short { .. }));
        let long = failing
            .iter()
            .any(|f| matches!(f.2, Problem::Long { .. } | Problem::Limit { .. }));
        match (short, long) {
            (true, false) => todo.push("lengthen it".into()),
            (false, true) => todo.push("shorten it or pick a lighter design".into()),
            _ => todo.push("change the length".into()),
        }
    }
    todo.push(if applied {
        "allow fewer rates".into()
    } else {
        "change the rate target".into()
    });
    let todo = join_or(&todo);
    let message = format!(
        "{} is {}. {}{}.",
        subject(d),
        join_and(&parts),
        todo[..1].to_uppercase(),
        &todo[1..]
    );
    Explanation {
        message,
        rates,
        fixes,
    }
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// The value in `lo..=hi` nearest `cur`, odd for an optimal design (its
/// even lengths are designed one tap shorter).
fn pick(cur: usize, lo: usize, hi: usize, odd: bool) -> Option<usize> {
    if lo > hi {
        return None;
    }
    let v = cur.clamp(lo, hi);
    if !odd || v % 2 == 1 {
        return Some(v);
    }
    if v < hi {
        Some(v + 1)
    } else if v > lo {
        Some(v - 1)
    } else {
        None
    }
}

/// The intersection of every case's interval for `make`, from `start`.
fn common(
    cases: &[Case],
    make: &dyn Fn(usize) -> FilterDesign,
    start: usize,
    min: usize,
    max: usize,
) -> Option<(usize, usize)> {
    let mut lo = min;
    let mut hi = max;
    for c in cases {
        let (l, h) = interval(&c.cx, make, start, min, max)?;
        lo = lo.max(l);
        hi = hi.min(h);
    }
    (lo <= hi).then_some((lo, hi))
}

fn builds_everywhere(d: &FilterDesign, cases: &[Case]) -> bool {
    cases.iter().all(|c| builds(d, &c.cx))
}

/// The passband end of a design with band edges, and `d` with it set.
fn passband(d: &FilterDesign) -> Option<f64> {
    match &kernel(d)?.shape {
        Shape::Optimal { passband_end, .. } => Some(*passband_end),
        Shape::WindowedSinc {
            cutoff: Cutoff::Band(b),
            ..
        } if b.passband_end_hz.is_none() => Some(b.passband_end),
        _ => None,
    }
}

fn with_passband(d: &FilterDesign, p: f64) -> FilterDesign {
    let mut d = d.clone();
    if let FilterDesign::Kernel(k) = &mut d {
        match &mut k.shape {
            Shape::Optimal { passband_end, .. } => *passband_end = p,
            Shape::WindowedSinc {
                cutoff: Cutoff::Band(b),
                ..
            } => b.passband_end = p,
            _ => {}
        }
    }
    d
}

/// The edits that make `d` build at every case, each held to `verify` there.
fn fixes(
    d: &FilterDesign,
    cases: &[Case],
    problems: &[Problem],
    verify: &dyn Fn(&FilterDesign, &BuildContext) -> bool,
) -> Vec<Fix> {
    let verified = |fix: &FilterDesign| cases.iter().all(|c| verify(fix, &c.cx));
    let mut out = Vec::new();
    let Some(k) = knob(d) else {
        return out;
    };
    let odd = optimal_method(d).is_some();
    // An automatic length (to the output's resolution).
    if !matches!(length(d), Some(Length::Auto(_))) {
        let auto = with_length(d, Length::Auto(FilterErrorBudget::MatchOutput));
        if builds_everywhere(&auto, cases) && verified(&auto) {
            out.push(Fix {
                label: "Use automatic length".into(),
                design: auto,
            });
        }
    }
    // A fixed number of taps, nearest the design's own.
    let first = cases.iter().find(|c| c.error.is_some()).map(|c| c.cx);
    let up = |cx: Option<BuildContext>| cx.map_or(1, |cx| cx.ratio.up.max(1) as usize);
    let (fixed_start, per_input_start) = match k {
        Knob::Fixed(n) => (n, (n / up(first)).max(1)),
        Knob::PerInput(t) => (t * up(first), t),
        Knob::Rule => (1023, 64),
    };
    if let Some((lo, hi)) = common(cases, &fixed_maker(d), fixed_start, 3, MAX_FIXED)
        && let Some(v) = pick(fixed_start, lo, hi, odd)
        && k != Knob::Fixed(v)
    {
        let fix = with_length(d, Length::Fixed(v));
        if builds_everywhere(&fix, cases) && verified(&fix) {
            out.push(Fix {
                label: format!("Set {} taps", thousands(v)),
                design: fix,
            });
        }
    }
    // Taps per input sample: the filter lasts as long at every ratio.
    if let Some((lo, hi)) = common(
        cases,
        &per_input_maker(d),
        per_input_start,
        1,
        MAX_PER_INPUT,
    ) && let Some(v) = pick(per_input_start, lo, hi, false)
        && k != Knob::PerInput(v)
    {
        let fix = with_length(d, Length::PerInput(Stepped::Fixed(v)));
        if builds_everywhere(&fix, cases) && verified(&fix) {
            out.push(Fix {
                label: format!("Set {} taps per input sample", thousands(v)),
                design: fix,
            });
        }
    }
    // A wider transition, when every refusal is a length too short for
    // the band.
    if problems.iter().all(|p| matches!(p, Problem::Short { .. }))
        && let Some(p0) = passband(d)
    {
        let ok = |p: f64| builds_everywhere(&with_passband(d, p), cases);
        let floor = p0 * 0.5;
        if ok(floor) {
            let (mut good, mut bad) = (floor, p0);
            for _ in 0..20 {
                let mid = 0.5 * (good + bad);
                if ok(mid) { good = mid } else { bad = mid }
            }
            // Down to the third decimal, so the field shows what builds.
            let p = (good * 1000.0).floor() / 1000.0;
            if p < p0 && ok(p) && verified(&with_passband(d, p)) {
                out.push(Fix {
                    label: format!("Widen the transition: pass to {p:.3}"),
                    design: with_passband(d, p),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
