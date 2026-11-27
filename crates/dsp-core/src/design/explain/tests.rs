use super::*;
use crate::design::paths::general::tests::{cx, sinc};
use crate::design::{Phase, Sampling, Window};

/// A length over the engine's longest kernel at one ratio is "over the
/// engine's longest kernel" there, and a shorter length fixes it.
#[test]
fn a_kernel_over_the_tap_limit_is_too_long() {
    let d = FilterDesign::Kernel(sinc(
        Window::Kaiser {
            beta: Some(14.0),
            sampling: Sampling::Symmetric,
        },
        Length::PerInput(Stepped::Fixed(60_000)),
        Cutoff::Fraction(Stepped::Fixed(0.95)),
        Phase::Intermediate {
            mix: 0.5,
            method: crate::design::PhaseMethod::Reflect,
        },
    ));
    let cases: Vec<Case> = [(44_100, 64), (96_000, 8)]
        .into_iter()
        .map(|(rate, up)| {
            let cx = cx(rate, RateRatio::integer(up));
            Case {
                error: crate::design::feasibility(&d, &cx).err(),
                cx,
            }
        })
        .collect();
    assert!(cases[0].error.is_some());
    assert!(cases[1].error.is_none(), "{:?}", cases[1].error);
    let x = explain(&d, &cases, true);
    eprintln!("{}", x.message);
    assert!(
        x.message.starts_with(
            "60,000 taps per input sample is over the engine's longest kernel at 44.1k ×64"
        ),
        "{}",
        x.message
    );
    let fix = x
        .fixes
        .iter()
        .find(|f| f.label.ends_with("per input sample"))
        .expect("a per-input fix");
    assert!(builds_everywhere(&fix.design, &cases));
}
