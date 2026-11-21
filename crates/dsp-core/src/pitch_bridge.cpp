#include "signalsmith-stretch/signalsmith-stretch.h"
#include <cstdint>

using Stretch64 = signalsmith::stretch::SignalsmithStretch<double>;

extern "C" {

void* av_pitch_new() {
    try {
        return new Stretch64();
    } catch (...) {
        return nullptr;
    }
}

void av_pitch_free(void* ptr) {
    if (!ptr) {
        return;
    }
    try {
        delete static_cast<Stretch64*>(ptr);
    } catch (...) {
    }
}

int32_t av_pitch_configure(
    void* ptr,
    int32_t channels,
    int32_t block_samples,
    int32_t interval_samples,
    bool split_computation
) {
    if (!ptr || channels <= 0 || block_samples <= 0 || interval_samples <= 0) {
        return -1;
    }
    try {
        static_cast<Stretch64*>(ptr)->configure(
            channels, block_samples, interval_samples, split_computation
        );
        return 0;
    } catch (...) {
        return -1;
    }
}

int32_t av_pitch_reset(void* ptr) {
    if (!ptr) {
        return -1;
    }
    try {
        static_cast<Stretch64*>(ptr)->reset();
        return 0;
    } catch (...) {
        return -1;
    }
}

int32_t av_pitch_set_transpose_semitones(
    void* ptr,
    double semitones,
    double tonality_limit
) {
    if (!ptr) {
        return -1;
    }
    try {
        static_cast<Stretch64*>(ptr)->setTransposeSemitones(semitones, tonality_limit);
        return 0;
    } catch (...) {
        return -1;
    }
}

int32_t av_pitch_input_latency(const void* ptr) {
    if (!ptr) {
        return -1;
    }
    try {
        return static_cast<const Stretch64*>(ptr)->inputLatency();
    } catch (...) {
        return -1;
    }
}

int32_t av_pitch_output_latency(const void* ptr) {
    if (!ptr) {
        return -1;
    }
    try {
        return static_cast<const Stretch64*>(ptr)->outputLatency();
    } catch (...) {
        return -1;
    }
}

int32_t av_pitch_process(
    void* ptr,
    const double* const* inputs,
    double* const* outputs,
    int32_t frames
) {
    if (!ptr || !inputs || !outputs || frames < 0) {
        return -1;
    }
    try {
        static_cast<Stretch64*>(ptr)->process(inputs, frames, outputs, frames);
        return 0;
    } catch (...) {
        return -1;
    }
}

namespace {

// The library treats input below its noise floor (energy 1e-15 over a call)
// as silence, and after two windows of it copies input to output, dropping
// whatever its overlap-add still holds. Above about 1.5x, two windows of
// input are less output time than the synthesis tail, so a sound that ends
// is cut short (by up to -42 dB of tail at 2x). While the tail still plays,
// the stretch's input is read through this view, which lifts the first
// sample of one silent call a window over the floor (by -146 dBFS, away
// from zero whatever its sign, so its magnitude always grows), so the
// library's silence count restarts before it reaches two windows and it
// keeps synthesising; once the tail is out, silence takes the shortcut as
// before.
constexpr double kTailLift = 5e-8;  // lift^2 = 2.5e-15 > the 1e-15 floor

struct LiftedChannel {
    const double* data;
    bool lift;
    double operator[](int i) const {
        if (lift && i == 0) {
            return data[0] + (data[0] < 0 ? -kTailLift : kTailLift);
        }
        return data[i];
    }
};

struct LiftedInputs {
    const double* const* inputs;
    bool lift;
    LiftedChannel operator[](int c) const {
        return LiftedChannel{inputs[c], lift && c == 0};
    }
};

double input_energy(const double* const* inputs, int channels, int frames) {
    double e = 0;
    for (int c = 0; c < channels; ++c) {
        for (int i = 0; i < frames; ++i) {
            e += inputs[c][i] * inputs[c][i];
        }
    }
    return e;
}

}  // namespace

// Time-stretch: `in_frames` input frames into `out_frames` output frames
// (the ratio sets the tempo; no transposition unless one was set).
// `quiet[0]` counts the output frames since the last input above the noise
// floor and `quiet[1]` the silent input frames since then or the last lift
// (the caller keeps them; reset them with the stretch). While `quiet[0]` is
// within `tail_frames`, silent input keeps the synthesis running (see
// `LiftedInputs`).
int32_t av_stretch_process(
    void* ptr,
    const double* const* inputs,
    int32_t in_frames,
    double* const* outputs,
    int32_t out_frames,
    int32_t channels,
    int64_t tail_frames,
    int64_t* quiet
) {
    if (!ptr || !inputs || !outputs || !quiet || in_frames < 0 || out_frames < 0
        || channels <= 0) {
        return -1;
    }
    try {
        auto* st = static_cast<Stretch64*>(ptr);
        bool silent = input_energy(inputs, channels, in_frames) < 1e-15;
        bool lift = false;
        if (!silent) {
            quiet[0] = 0;
            quiet[1] = 0;
        } else {
            // The library takes the shortcut once its count of silent input
            // reaches two windows: lift a window short of that.
            quiet[1] += in_frames;
            lift = in_frames > 0 && quiet[0] < tail_frames && quiet[1] >= st->blockSamples();
            if (lift) {
                quiet[1] = 0;
            }
            quiet[0] += out_frames;
        }
        st->process(
            LiftedInputs{inputs, lift}, in_frames, outputs, out_frames
        );
        return 0;
    } catch (...) {
        return -1;
    }
}

} // extern "C"
