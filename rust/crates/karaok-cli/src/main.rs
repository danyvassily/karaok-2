//! Diagnostic, Core Audio inventory, and reproducible RT DSP benchmark CLI for Karaok Party (§11.4, §13).

use karaok_audio_device::default_system_backend;
use karaok_audio_engine::AudioEngineHandle;
use karaok_diagnostics::RtCounters;
use karaok_types::{MixParams, MusicalScale, PitchClass, VoiceDspParams};
use karaok_voice_dsp::{VoiceDspChain, YinDetector};
use std::env;
use std::f32::consts::TAU;
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("devices") => cmd_devices(),
        Some("bench-dsp") => cmd_bench_dsp(),
        _ => {
            eprintln!("Usage: karaok-cli <devices | bench-dsp>");
            std::process::exit(1);
        }
    }
}

fn cmd_devices() {
    let hal = default_system_backend();
    match hal.enumerate_devices() {
        Ok(devs) => {
            println!("=== Inventaire Core Audio HAL (UID expurgés localement) ===");
            println!("Entrées exposées :");
            for d in devs.iter().filter(|d| d.has_input()) {
                println!(
                    "  - [{}] transport={} ch_in={} sr={:?} buf={:?} lat_in={:?} default={} nothing_a_candidate={}",
                    d.redacted_uid,
                    d.transport.label_fr(),
                    d.input_channels,
                    d.nominal_sample_rate_hz,
                    d.buffer_frame_size,
                    d.input_latency_frames,
                    d.is_default_input,
                    d.nothing_headphone_a_candidate
                );
            }
            println!("Sorties exposées :");
            for d in devs.iter().filter(|d| d.has_output()) {
                println!(
                    "  - [{}] transport={} ch_out={} sr={:?} buf={:?} lat_out={:?} default={} nothing_a_candidate={}",
                    d.redacted_uid,
                    d.transport.label_fr(),
                    d.output_channels,
                    d.nominal_sample_rate_hz,
                    d.buffer_frame_size,
                    d.output_latency_frames,
                    d.is_default_output,
                    d.nothing_headphone_a_candidate
                );
            }
        }
        Err(e) => {
            eprintln!("Erreur HAL : {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_bench_dsp() {
    let sr = 48_000.0_f32;
    let block_frames = 256usize;
    let budget_us = (block_frames as f64 / sr as f64) * 1_000_000.0; // 5333.33 us

    // 1. Pitch correction accuracy benchmark on detuned harmonic voice (431 Hz -> A4 = 440 Hz)
    let mut vp = VoiceDspParams::preset_studio();
    vp.tonic = PitchClass::A;
    vp.scale = MusicalScale::Major;
    vp.pitch_amount = 1.0;
    vp.pitch_speed = 0.90;
    vp.neutral_zone_cents = 0.0;
    vp.monitoring_enabled = true;
    vp.monitoring_level = 0.8;

    let mut dsp = VoiceDspChain::new(sr, vp);
    let mut in_block = vec![0.0_f32; block_frames];
    let mut out_l = vec![0.0_f32; block_frames];
    let mut out_r = vec![0.0_f32; block_frames];
    let mut verifier = YinDetector::new(sr);

    // Warm up + measure 2000 blocks (~10.6 seconds of audio)
    let total_blocks = 2000usize;
    let mut timings_us: Vec<f64> = Vec::with_capacity(total_blocks);
    let mut corrected_f0_samples: Vec<f32> = Vec::new();
    let mut phase_sample = 0usize;

    for b in 0..total_blocks {
        for slot in &mut in_block {
            let t = phase_sample as f32 / sr;
            *slot = 0.50 * (TAU * 431.0 * t).sin()
                + 0.22 * (TAU * 862.0 * t).sin()
                + 0.10 * (TAU * 1293.0 * t).sin();
            phase_sample += 1;
        }
        let t0 = Instant::now();
        let _ = dsp.process_block(&in_block, &mut out_l, &mut out_r);
        let dt_us = t0.elapsed().as_nanos() as f64 / 1000.0;
        timings_us.push(dt_us);

        if b >= 120 {
            for &s in &out_l {
                let e = verifier.push_sample(s);
                if e.voiced {
                    corrected_f0_samples.push(e.f0_hz);
                }
            }
        }
    }

    timings_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = timings_us[timings_us.len() / 2];
    let p95 = timings_us[(timings_us.len() * 95) / 100];
    let p99 = timings_us[(timings_us.len() * 99) / 100];
    let max = *timings_us.last().unwrap();

    corrected_f0_samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median_f0 = corrected_f0_samples
        .get(corrected_f0_samples.len() / 2)
        .copied()
        .unwrap_or(0.0);

    // 2. Full RT engine render callback benchmark + simulated 60-minute endurance (+50 ppm drift)
    let counters = Arc::new(RtCounters::new());
    let (mut eng, mut rt_out, mut rt_mic) = AudioEngineHandle::create(
        48_000,
        48_000,
        vp,
        MixParams::default(),
        Arc::clone(&counters),
    );
    eng.set_mic_capture_active(true);

    // Run 30,000 blocks (~2.67 minutes real-time equivalent at 48 kHz / 256 frames) in CLI bench with +50ppm drift
    let endurance_blocks = 30_000usize;
    let mut eng_timings_us = Vec::with_capacity(endurance_blocks);
    let mut out_stereo = vec![0.0_f32; block_frames * 2];
    let mut drift_acc = 0.0_f64;

    for _ in 0..endurance_blocks {
        drift_acc += block_frames as f64 * 1.00005; // +50 ppm clock drift
        let push_n = drift_acc.floor() as usize;
        drift_acc -= push_n as f64;
        if push_n <= in_block.len() {
            rt_mic.push_mono(&in_block[..push_n]);
        } else {
            rt_mic.push_mono(&in_block);
            rt_mic.push_mono(&in_block[..(push_n - in_block.len()).min(in_block.len())]);
        }

        let t0 = Instant::now();
        rt_out.render_stereo_interleaved(&mut out_stereo, block_frames);
        eng_timings_us.push(t0.elapsed().as_nanos() as f64 / 1000.0);
    }

    eng_timings_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let ep50 = eng_timings_us[eng_timings_us.len() / 2];
    let ep95 = eng_timings_us[(eng_timings_us.len() * 95) / 100];
    let ep99 = eng_timings_us[(eng_timings_us.len() * 99) / 100];
    let emax = *eng_timings_us.last().unwrap();
    let csnap = counters.snapshot();

    println!("=== Benchmark DSP & Moteur RT Karaok Party (48 kHz, bloc 256 frames, budget {budget_us:.2} µs) ===");
    println!("Latence algorithmique VoiceDspChain : {:.2} ms ({} samples)", dsp.algorithmic_latency_ms(), dsp.algorithmic_latency_samples());
    println!("Latence algorithmique totale (DSP + Limiteur) : {:.2} ms", eng.voice_dsp_latency_ms());
    println!("Correction justesse (entrée 431.0 Hz -> cible A4 440.0 Hz) : médiane mesurée = {median_f0:.2} Hz");
    println!("VoiceDspChain (2000 blocs) : p50={p50:.1} µs | p95={p95:.1} µs | p99={p99:.1} µs ({:.2}% budget) | max={max:.1} µs", (p99 / budget_us) * 100.0);
    println!("RtOutputProcessor ({endurance_blocks} blocs, +50ppm drift) : p50={ep50:.1} µs | p95={ep95:.1} µs | p99={ep99:.1} µs ({:.2}% budget) | max={emax:.1} µs", (ep99 / budget_us) * 100.0);
    println!(
        "Compteurs RT : callbacks={} deadline_overruns={} mic_underruns={} mic_overruns={} non_finite={} occ={} ratio={:.6}",
        csnap.output_callbacks,
        csnap.callback_deadline_overruns,
        csnap.mic_underruns,
        csnap.mic_overruns,
        csnap.non_finite_sanitizations,
        csnap.mic_ring_occupancy_frames,
        csnap.mic_resample_ratio
    );
}
