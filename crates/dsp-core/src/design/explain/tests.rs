use super::*;
use crate::design::paths::general::tests::{cx, sinc};
use crate::design::{Phase, Sampling, Window};

fn optimal(length: Length) -> FilterDesign {
    FilterDesign::Kernel(KernelDesign {
        shape: Shape::Optimal {
            method: OptimalMethod::Equiripple,
            length,
            passband_end: 0.9,
            stopband_start: 1.0,
        },
        ..sinc(
            Window::Hann {
                sampling: Sampling::Symmetric,
            },
            Length::PerInput(Stepped::Fixed(1)),
            Cutoff::Fraction(Stepped::Fixed(1.0)),
            Phase::Linear,
        )
    })
}

/// "Highest, same family, up to 768k" at the rates of a refused case.
fn cases(d: &FilterDesign) -> Vec<Case> {
    [
        (44_100, 16),
        (48_000, 16),
        (88_200, 8),
        (96_000, 8),
        (176_400, 4),
        (352_800, 2),
    ]
    .into_iter()
    .map(|(rate, up)| {
        let cx = cx(rate, RateRatio::integer(up));
        Case {
            error: crate::design::feasibility(d, &cx).err(),
            cx,
        }
    })
    .collect()
}

/// A refused case: 1,279 equiripple taps, too short at x16 and too deep at
/// x4 and x2, said plainly, grouped, with fixes that build at every rate.
#[test]
fn a_fixed_equiripple_length_is_explained_and_fixed() {
    let d = optimal(Length::Fixed(1279));
    let cases = cases(&d);
    assert!(cases.iter().any(|c| c.error.is_some()));
    assert!(cases.iter().any(|c| c.error.is_none()));
    let t0 = std::time::Instant::now();
    let x = explain(&d, &cases, true);
    let took = t0.elapsed();
    eprintln!("{}\n{:#?}\n{took:?}", x.message, x.rates);
    assert!(
        x.message
            .starts_with("1,279 taps is too short at 44.1k ×16"),
        "{}",
        x.message
    );
    assert!(
        x.message.contains("too long for an equiripple design at"),
        "{}",
        x.message
    );
    assert!(
        x.message.ends_with("or allow fewer rates."),
        "{}",
        x.message
    );
    let short = x.rates.iter().find(|r| r.source_rate == 44_100).unwrap();
    assert!(
        short.short.starts_with("too short here (needs about "),
        "{}",
        short.short
    );
    assert!(short.short.ends_with(" taps)"), "{}", short.short);
    assert!(short.detail.contains("polyphase"), "{}", short.detail);
    assert!(!x.fixes.is_empty());
    for f in &x.fixes {
        eprintln!("fix: {}", f.label);
        for c in &cases {
            assert!(builds(&f.design, &c.cx), "{} at {:?}", f.label, c.cx.ratio);
        }
    }
    assert!(
        x.fixes
            .iter()
            .any(|f| f.label.ends_with("per input sample"))
    );
    assert!(took < std::time::Duration::from_secs(2), "{took:?}");
}

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

#[test]
fn labels() {
    assert_eq!(thousands(1279), "1,279");
    assert_eq!(thousands(8_388_608), "8,388,608");
    assert_eq!(about(1537), 1600);
    assert_eq!(about(87), 87);
    assert_eq!(about(1600), 1600);
    assert_eq!(rate_label(44_100), "44.1k");
    assert_eq!(rate_label(352_800), "352.8k");
    assert_eq!(
        ratio_label(RateRatio::from_rates(44_100, 48_000)),
        "×160/147"
    );
    assert_eq!(
        join_and(&["a".into(), "b".into(), "c".into()]),
        "a, b and c"
    );
}
