//! Reference EQ responses from `audivane-dsp-core`, used by the webview's
//! `eqmath.test.ts` to prove the TS curve matches the engine.
//! Regenerate: `npm run gen:eq-fixture` in `app/`.

use audivane_dsp_ui::eq::{auto_preamp_db, band_db, log_space, response_db};
use audivane_protocol::{Band, FilterType};
use serde_json::json;

fn b(kind: FilterType, f: f64, g: f64, q: f64, enabled: bool) -> Band {
    Band {
        kind,
        freq_hz: f,
        gain_db: g,
        q,
        enabled,
    }
}

fn main() {
    use FilterType::*;
    let sets: Vec<(&str, f64, f64, Vec<Band>)> = vec![
        (
            "autoeq-hd650",
            48_000.0,
            -6.8,
            vec![
                b(LowShelf, 105.0, 5.5, 0.7, true),
                b(Peaking, 57.0, 1.8, 0.9, true),
                b(Peaking, 153.0, -1.0, 1.02, true),
                b(Peaking, 1280.0, -1.1, 2.9, true),
                b(Peaking, 2550.0, 1.6, 1.95, true),
                b(Peaking, 4480.0, 1.5, 4.0, true),
                b(HighShelf, 10_000.0, -2.5, 0.7, true),
            ],
        ),
        (
            "every-type-44k1",
            44_100.0,
            0.0,
            vec![
                b(LowPass, 18_000.0, 0.0, 0.7, true),
                b(HighPass, 25.0, 0.0, 0.5, true),
                b(BandPass, 1000.0, 0.0, 2.0, true),
                b(Notch, 3000.0, 0.0, 8.0, true),
                b(AllPass, 500.0, 0.0, 1.0, true),
                b(Peaking, 200.0, 9.0, 0.3, false),
            ],
        ),
        (
            "high-q-96k",
            96_000.0,
            -2.0,
            vec![
                b(Peaking, 20.0, 12.0, 10.0, true),
                b(Peaking, 6000.0, -12.0, 20.0, true),
                b(HighShelf, 40_000.0, 3.0, 0.7, true),
                b(LowShelf, 30.0, -6.0, 1.4, true),
            ],
        ),
        (
            "degenerate",
            48_000.0,
            0.0,
            vec![
                b(Peaking, 30_000.0, 6.0, 1.0, true),
                b(Peaking, 1000.0, 6.0, 0.0, true),
                b(Peaking, 1000.0, 3.0, 1.0, true),
            ],
        ),
    ];
    let freqs = log_space(10.0, 23_000.0, 64);
    let cases: Vec<_> = sets
        .into_iter()
        .map(|(name, fs, preamp, bands)| {
            json!({
                "name": name,
                "sample_rate": fs,
                "preamp_db": preamp,
                "bands": bands,
                "freqs": freqs,
                "total_db": response_db(&bands, preamp, fs, &freqs),
                "band_db": bands.iter().map(|x| band_db(x, fs, &freqs)).collect::<Vec<_>>(),
                "auto_preamp_db": auto_preamp_db(&bands, fs),
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({ "cases": cases })).unwrap()
    );
}
