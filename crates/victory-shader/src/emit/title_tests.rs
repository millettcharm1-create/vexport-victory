//! Programs of a title run on the host GPU with data of our own, against what the same
//! arithmetic gives on the CPU: for passes whose result other passes look colours up in,
//! where a wrong table shows only as a wrong picture. The containers are the title's (rule
//! #1): each test takes their paths from an environment variable and is skipped without it.

use super::host::{Dispatch, Host, Root, Stride, View};
use super::lower;

/// A cubic Bezier's value at `t`.
fn bezier(p: [f32; 4], t: f64) -> f64 {
    let [a, b, c, d] = p.map(f64::from);
    let s = 1.0 - t;
    s * s * s * a + 3.0 * s * s * t * b + 3.0 * s * t * t * c + t * t * t * d
}

/// The curve of `segments` (each four x and four y, x never falling) at `x`: the segment
/// whose x holds it, the `t` where its x is `x`, its y there.
fn curve(segments: &[[[f32; 4]; 2]], x: f32) -> f32 {
    let segment = segments.iter().find(|s| x < s[0][3]).unwrap_or(segments.last().unwrap());
    let (mut low, mut high) = (0.0f64, 1.0f64);
    for _ in 0..60 {
        let mid = 0.5 * (low + high);
        if bezier(segment[0], mid) < f64::from(x) { low = mid } else { high = mid }
    }
    bezier(segment[1], 0.5 * (low + high)) as f32
}

/// A title's container translated for a host of 32-lane waves, as an RTX card is.
fn translated(path: &str) -> super::HostShader {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let shader = super::translate(&bytes);
    lower::TEST_HOST_WAVE.set(was);
    shader.unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Numbers in 0..1 in a fixed order.
fn random() -> impl FnMut() -> f32 {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// The title's passes that bake a curve of two-dimensional Bezier segments into a row of
/// 4,096 values (each value's cubic solved in closed form: a cube root, or three cosines
/// where it has three roots): what the card writes is the curve, for curves of many shapes.
/// The final picture takes its exposure and each channel's tone from two such rows. The
/// closed form is the title's, in single precision: where a segment's x crowd at one end
/// it is off by up to a hundredth for some dozens of values (0.0083 here), and a value or
/// two of a curve may be far off, where a segment begins and the form divides by what is
/// nearly nothing (seen once in 48 curves, at the value just above a segment's first x).
/// So the third furthest value of each curve is the judge, and a hundredth the bound: a
/// row of plateaus, or of another curve, is far outside it.
///
/// `VICTORY_TEST_SPLINE_BAKES` lists the containers with their numbers of segments,
/// `2=<path>;3=<path>`; run with `-- --ignored --nocapture`.
#[test]
#[ignore = "needs a title's containers"]
fn a_baked_bezier_curve_is_the_curve() {
    let Ok(list) = std::env::var("VICTORY_TEST_SPLINE_BAKES") else {
        eprintln!("VICTORY_TEST_SPLINE_BAKES is not set; not run");
        return;
    };
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    const BOUND: f32 = 1e-2;
    let mut random = random();
    let mut ran = 0;
    for entry in list.split(';').filter(|e| !e.is_empty()) {
        let (n, path) = entry.split_once('=').expect("<segments>=<path>");
        let n: usize = n.trim().parse().expect("a number of segments");
        let shader = translated(path);
        let (root, pso) = host.compute_pipeline(&shader).unwrap_or_else(|e| panic!("{path}: {e}"));
        let mut worst = (0.0f32, String::new());
        for shape in 0..48 {
            // x from 0 to 1 over the segments, never falling; y anything in 0..1. The
            // first shapes have evenly spaced x (the cubic falls to a line), the next
            // ones x that crowd (three roots), the rest whatever comes.
            let mut segments = Vec::new();
            let mut x = 0.0f32;
            for s in 0..n {
                let end = if s + 1 == n { 1.0 } else { x + (1.0 - x) * (0.2 + 0.6 * random()) / (n - s) as f32 * 1.5 };
                let end = end.min(1.0);
                let span = end - x;
                let (a, b) = match shape {
                    0..=3 => (1.0 / 3.0, 2.0 / 3.0),
                    4..=15 => {
                        // (Crowded: the second inner point may come out before the first.)
                        let (a, b) = (0.8 + 0.2 * random(), 0.2 * random());
                        (a.min(b), a.max(b))
                    }
                    _ => {
                        let a = random();
                        (a, a + (1.0 - a) * random())
                    }
                };
                let xs = [x, x + span * a, x + span * b, end];
                let ys = [random(), random(), random(), random()];
                segments.push([xs, ys]);
                x = end;
            }
            let points: Vec<u32> = segments.iter().flatten().flatten().map(|v| v.to_bits()).collect();
            let constants = [(1.0f32 / 4095.0).to_bits(), 4096, 1.0f32.to_bits(), 0.0f32.to_bits()];
            let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(ud))],
                        views: &[View::Buffer { data: &points, stride: Stride::Raw }, View::LineUav(4096), View::Cbv(&constants)],
                        samplers: &[],
                        groups: [32, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{path}: {e}"));
            let got: Vec<f32> = out[0].iter().map(|&w| f32::from_bits(w)).collect();
            assert_eq!(got.len(), 4096);
            let mut off: Vec<(f32, usize)> = got
                .iter()
                .enumerate()
                .map(|(i, &g)| {
                    let d = (g - curve(&segments, i as f32 * (1.0 / 4095.0))).abs();
                    (if d.is_nan() { f32::INFINITY } else { d }, i)
                })
                .collect();
            off.sort_by(|a, b| b.0.total_cmp(&a.0));
            let wrong = off.iter().take_while(|o| o.0 > BOUND).count();
            let (d, at) = off[2];
            if d > worst.0 || worst.1.is_empty() {
                let x = at as f32 / 4095.0;
                worst = (
                    d,
                    format!(
                        "shape {shape}: {wrong} of 4096 values off; the third furthest by {d} at {at} (x {x}): {} for {}; segments {segments:?}",
                        got[at],
                        curve(&segments, x)
                    ),
                );
            }
        }
        eprintln!("{n} segments: 48 curves; the worst: {}", worst.1);
        assert!(worst.0 <= BOUND, "{n} segments: {}", worst.1);
        ran += 1;
    }
    assert!(ran > 0, "no container listed");
}

/// The shaper the title's colour tables are indexed and filled through: 14 stops of linear
/// light around mid grey as 0..1, with a small linear toe. Its constants are the pass's own.
mod shaper {
    pub const STOPS: f64 = 14.0;
    pub const MIDDLE: f64 = 0.418_377_3;
    pub const GREY: f64 = 0.18;
    pub const TOE: f64 = 0.003_105;

    /// Linear light of a table value.
    pub fn light(v: f64) -> f64 {
        GREY * (STOPS * (v - MIDDLE)).exp2() - TOE
    }

    /// The table value of linear light.
    pub fn value(light: f64) -> f64 {
        (light + TOE).max(1e-30).log2() / STOPS + (MIDDLE - GREY.log2() / STOPS)
    }
}

/// A cubic table of `side` texels a side, x fastest.
struct Table {
    side: u32,
    texels: Vec<[f32; 4]>,
}

impl Table {
    /// `colour` of each texel's place in 0..1 (the first texel 0, the last 1).
    fn of(side: u32, colour: impl Fn([f64; 3]) -> [f64; 3]) -> Table {
        let at = |i: u32| f64::from(i) / f64::from(side - 1);
        let mut texels = Vec::with_capacity((side * side * side) as usize);
        for z in 0..side {
            for y in 0..side {
                for x in 0..side {
                    let [r, g, b] = colour([at(x), at(y), at(z)]);
                    texels.push([r as f32, g as f32, b as f32, 1.0]);
                }
            }
        }
        Table { side, texels }
    }

    /// The table between its texels at `place` (0..1 from the first texel's centre to the
    /// last one's), as a sampler that blends the eight texels around gives it.
    fn at(&self, place: [f64; 3]) -> [f64; 3] {
        let last = f64::from(self.side - 1);
        let p = place.map(|p| (p * last).clamp(0.0, last));
        let low = p.map(|p| (p.floor() as u32).min(self.side - 2));
        let part = [0, 1, 2].map(|i| p[i] - f64::from(low[i]));
        let mut out = [0.0; 3];
        for corner in 0..8u32 {
            let c = [0, 1, 2].map(|i| corner >> i & 1);
            let weight: f64 = (0..3).map(|i| if c[i] == 1 { part[i] } else { 1.0 - part[i] }).product();
            let texel =
                self.texels[(((low[2] + c[2]) * self.side + low[1] + c[1]) * self.side + low[0] + c[0]) as usize];
            for (o, t) in out.iter_mut().zip(texel) {
                *o += weight * f64::from(t);
            }
        }
        out
    }
}

/// The title's pass that blends up to four colour grading tables over a base table into
/// the 33-texel table the final picture is looked up in (each texel: the base table's
/// colour there, each layer looked up at that colour, the weighted sum in linear light):
/// what the card writes is that blend with every layer read between its texels. A layer
/// read at its nearest texel instead gives a table of plateaus, and a picture whose every
/// channel has a dozen levels (the banding of 2026-11-26, in rooms and cutscenes, where
/// the title has layers on).
///
/// `VICTORY_TEST_TABLE_BLEND` is the container's path; run with `-- --ignored --nocapture`.
#[test]
#[ignore = "needs a title's container"]
fn a_blend_of_colour_tables_reads_each_between_its_texels() {
    let Ok(path) = std::env::var("VICTORY_TEST_TABLE_BLEND") else {
        eprintln!("VICTORY_TEST_TABLE_BLEND is not set; not run");
        return;
    };
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let shader = translated(&path);
    let (root, pso) = host.compute_pipeline(&shader).unwrap_or_else(|e| panic!("{path}: {e}"));
    const SIDE: u32 = 33;
    // Where the pass finds its views, in descriptors from its table's start, and its
    // constants, in vectors of four: the layers' weights, the base's, the table's step
    // and side, each layer's and the base's scale and offset from a colour to a place.
    const LAYERS: [usize; 4] = [3, 0, 7, 10];
    const BASE: usize = 1;
    const OUT: usize = 16;
    const CONSTANTS: usize = 17;
    const WEIGHTS: usize = 0;
    const BASE_WEIGHT: usize = 16;
    const STEP: usize = 114;
    const PLACES: usize = 153;
    const BASE_PLACE: usize = 168;
    // Cases: the base table, the layers (side, colours) and the weights (base, layers).
    type Colour = fn([f64; 3]) -> [f64; 3];
    let identity: Colour = |c| c;
    let curved: Colour = |[r, g, b]| [r.powf(1.3), 0.9 * g + 0.1 * b * b, b.sqrt() * 0.7 + 0.3 * r];
    let warm: Colour = |[r, g, b]| [(1.1 * r).min(1.0), g * g * (3.0 - 2.0 * g), 0.8 * b + 0.1 * r];
    let cool: Colour = |[r, g, b]| [0.9 * r + 0.05, 0.5 * (g + r), b.powf(0.8)];
    let cases: [(&str, Colour, [(u32, Colour); 4], (f32, [f32; 4])); 3] = [
        ("one 17-texel layer alone", identity, [(17, identity), (33, identity), (33, identity), (33, identity)], (0.0, [1.0, 0.0, 0.0, 0.0])),
        ("one 33-texel layer over a curved base", curved, [(17, identity), (33, warm), (33, identity), (33, identity)], (0.0, [0.0, 1.0, 0.0, 0.0])),
        ("four layers and the base", curved, [(17, warm), (33, cool), (33, identity), (17, curved)], (0.2, [0.3, 0.2, 0.1, 0.2])),
    ];
    for half in [true, false] {
        for (what, base, layers, (base_weight, weights)) in &cases {
            let base = Table::of(SIDE, base);
            let layers = layers.map(|(side, colour)| Table::of(side, colour));
            let mut constants = vec![0u32; 4 * 169];
            let mut set = |vector: usize, values: &[u32]| constants[4 * vector..][..values.len()].copy_from_slice(values);
            let place = |side: u32| [(1.0 - 1.0 / side as f32).to_bits(), (0.5 / side as f32).to_bits()];
            for (i, layer) in layers.iter().enumerate() {
                set(WEIGHTS + i, &[weights[i].to_bits()]);
                set(PLACES + i, &place(layer.side));
            }
            set(BASE_WEIGHT, &[base_weight.to_bits()]);
            set(STEP, &[(1.0 / (SIDE - 1) as f32).to_bits(), SIDE]);
            set(BASE_PLACE, &place(SIDE));
            let nothing = [0u32; 4];
            let mut views: Vec<View> = (0..=CONSTANTS).map(|_| View::Cbv(&nothing)).collect();
            views[BASE] = View::Volume { side: base.side, texels: &base.texels, half };
            for (slot, layer) in LAYERS.iter().zip(&layers) {
                views[*slot] = View::Volume { side: layer.side, texels: &layer.texels, half };
            }
            views[OUT] = View::VolumeUav { side: SIDE, half };
            views[CONSTANTS] = View::Cbv(&constants);
            let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(ud))],
                        views: &views,
                        samplers: &[],
                        groups: [SIDE.div_ceil(8), SIDE.div_ceil(8), SIDE],
                    },
                )
                .unwrap_or_else(|e| panic!("{path}: {e}"));
            let got = &out[0];
            assert_eq!(got.len(), (SIDE * SIDE * SIDE * 4) as usize);
            // (furthest off, where, what was written, what the blend is)
            let mut worst = (0.0f64, [0u32; 3], [0.0f32; 3], [0.0f64; 3]);
            let mut wrong = 0u32;
            for (i, texel) in base.texels.iter().enumerate() {
                let colour = [0, 1, 2].map(|c| f64::from(texel[c]));
                let mut light = colour.map(|v| f64::from(*base_weight) * shaper::light(v) + shaper::TOE);
                for (layer, weight) in layers.iter().zip(weights) {
                    let there = layer.at(colour);
                    for c in 0..3 {
                        light[c] += f64::from(*weight) * shaper::light(there[c]);
                    }
                }
                let want = light.map(|l| shaper::value(l - shaper::TOE));
                let wrote = [0, 1, 2].map(|c| f32::from_bits(got[4 * i + c]));
                let off = (0..3).map(|c| (f64::from(wrote[c]) - want[c]).abs()).fold(0.0, f64::max);
                let off = if off.is_nan() { f64::INFINITY } else { off };
                wrong += u32::from(off > 4e-3);
                if off > worst.0 {
                    let i = i as u32;
                    worst = (off, [i % SIDE, i / SIDE % SIDE, i / (SIDE * SIDE)], wrote, want);
                }
            }
            let format = if half { "half floats" } else { "floats" };
            eprintln!(
                "{what}, {format}: {wrong} of {} texels off; the furthest by {:.5} at {:?}: {:?} for {:?}",
                base.texels.len(),
                worst.0,
                worst.1,
                worst.2,
                worst.3
            );
            assert!(worst.0 <= 4e-3, "{what}, {format}: off by {} at {:?}", worst.0, worst.1);
        }
    }
}

/// The title's pass that bakes its colour grading adjustments into a 33-texel table
/// (`postfx_lut:CS_LUT3DBakeColorGradingAdjustments`), run with the inputs of one dispatch
/// a capture saw: its 2x2 texture of scene statistics (RGBA32F), its two constant buffers
/// (128 and 2,704 bytes). The table it writes goes to a file, to be compared with the one
/// the running title made from the same inputs.
///
/// `VICTORY_TEST_GRADING_BAKE` is `<container>;<inputs>;<output>`: the inputs are 16
/// floats, 32 and 676 dwords, little-endian, one after another; the output gets the
/// table's 33x33x33 RGBA texels as floats, x fastest. Both files belong outside the
/// repository. Run with `-- --ignored --nocapture`.
#[test]
#[ignore = "needs a title's container and a capture's inputs"]
fn a_grading_bake_runs_as_in_a_capture() {
    let Ok(spec) = std::env::var("VICTORY_TEST_GRADING_BAKE") else {
        eprintln!("VICTORY_TEST_GRADING_BAKE is not set; not run");
        return;
    };
    let [container, inputs, output]: [&str; 3] = spec.split(';').collect::<Vec<_>>().try_into().expect("three paths");
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let words: Vec<u32> = std::fs::read(inputs)
        .expect("the inputs")
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    assert_eq!(words.len(), 16 + 32 + 676, "16 floats, 32 and 676 dwords");
    let texels: Vec<[f32; 4]> =
        words[..16].chunks_exact(4).map(|t| [0, 1, 2, 3].map(|i| f32::from_bits(t[i]))).collect();
    let shader = translated(container);
    let (root, pso) = host.compute_pipeline(&shader).unwrap_or_else(|e| panic!("{container}: {e}"));
    let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[
                    View::Texture(2, 2, &texels),
                    View::VolumeUav { side: 33, half: true },
                    View::Cbv(&words[16..48]),
                    View::Cbv(&words[48..]),
                ],
                samplers: &[],
                groups: [5, 5, 33],
            },
        )
        .unwrap_or_else(|e| panic!("{container}: {e}"));
    let bytes: Vec<u8> = out[0].iter().flat_map(|w| w.to_le_bytes()).collect();
    std::fs::write(output, bytes).expect("the output");
    eprintln!("wrote {} texels to {output}", out[0].len() / 4);
}
