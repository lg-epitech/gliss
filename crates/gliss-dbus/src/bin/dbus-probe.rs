//! Explicit private-fixture probe; no daemon/guest/QMP launch or automatic discovery.
use anyhow::{Context, Result, ensure};
use gliss_dbus::{
    CaptureOptions, DbusSource, connect_private_bus, connect_private_bus_with_options,
};
use std::time::{Duration, Instant};
use tokio::time::timeout;

// A counter record is emitted at the deadline even if no event arrives. Zero
// calls and array fallback therefore remain visible to the owning harness.
fn print_map_stats(source: &DbusSource, phase: &str) -> Result<()> {
    let stats = source.map_stats()?;
    let description = stats.last_description.as_ref().map(|description| {
        serde_json::json!({
            "device":description.device,"inode":description.inode,
            "file_extent":description.file_extent,"offset":description.offset,
            "width":description.width,"height":description.height,
            "stride":description.stride,"pixman_format":format!("0x{:08x}",description.format),
        })
    });
    println!(
        "{}",
        serde_json::json!({
            "kind":"map-stats","phase":phase,"advertised":stats.advertised,
            "scanout_map_calls":stats.scanout_map_calls,"update_map_calls":stats.update_map_calls,
            "scanout_maps":stats.scanout_maps,"update_maps":stats.update_maps,
            "scanout_map_errors":stats.scanout_map_errors,"update_map_errors":stats.update_map_errors,
            "replacements":stats.replacements,"released_generations":stats.released_generations,
            "invalidations":stats.invalidations,"active_generation":stats.active_generation,
            "last_description":description,"last_damage":stats.last_damage,"last_error":stats.last_error,
            "array_scanout_calls":stats.array_scanout_calls,"array_update_calls":stats.array_update_calls,
            "array_scanouts":stats.array_scanouts,"array_updates":stats.array_updates,
            "array_bytes":stats.array_bytes,"read_calls":stats.read_calls,"bytes_read":stats.bytes_read,
            "read_convert_ms_total":stats.read_convert_ns as f64 / 1_000_000.0,
            "map_callbacks_observed":stats.scanout_maps > 0 && stats.update_maps > 0,
            "array_transport_observed":stats.array_scanout_calls > 0 || stats.array_update_calls > 0,
            "image_coherence_proved":false,
        })
    );
    Ok(())
}

async fn close_map_source(source: DbusSource, phase: &str) -> Result<()> {
    let result = source.close_checked().await;
    let error = result.as_ref().err().map(|error| format!("{error:#}"));
    println!(
        "{}",
        serde_json::json!({
            "kind":"map-close","phase":phase,"cleanup_complete":result.is_ok(),"error":error,
        })
    );
    result
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let unix_map = args
        .iter()
        .filter(|value| value.as_str() == "--unix-map")
        .count();
    ensure!(unix_map <= 1, "--unix-map may appear once");
    args.retain(|value| value != "--unix-map");
    let unix_map = unix_map == 1;
    let hold_only = args
        .iter()
        .filter(|value| value.as_str() == "--hold-only")
        .count();
    ensure!(hold_only <= 1, "--hold-only may appear once");
    args.retain(|value| value != "--hold-only");
    let hold_only = hold_only == 1;
    let mut args = args.into_iter();
    let address = args.next().context(
        "usage: dbus-probe <private-unix-address> <expected-vm-uuid> [console-id] [seconds] [--hold-only] [--unix-map]",
    )?;
    let vm_uuid = args.next().context("expected VM UUID is required")?;
    let console = args
        .next()
        .map(|value| value.parse::<u32>())
        .transpose()?
        .unwrap_or(0);
    let seconds = args
        .next()
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(5);
    ensure!(
        args.next().is_none() && (1..=if hold_only { 600 } else { 60 }).contains(&seconds),
        "probe duration must be 1..=60 seconds, or 1..=600 for --hold-only"
    );
    let mut source = if unix_map {
        connect_private_bus_with_options(
            &address,
            &vm_uuid,
            console,
            CaptureOptions { unix_map: true },
        )
        .await?
    } else {
        connect_private_bus(&address, &vm_uuid, console).await?
    };
    println!(
        "{}",
        serde_json::json!({"kind":"connected","transport":"private-unix-qemu-dbus",
        "mode":if hold_only { "hold-only" } else { "frames" },"frame_snapshots":!hold_only,
        "frame_hashing":!hold_only,"callbacks_normalize":true,
        "vm_uuid":vm_uuid,"console":console,"input":"separate-rfb","host_arch":std::env::consts::ARCH,
        "pixel_byte_order":if cfg!(target_endian = "little") { "little" } else { "big" }})
    );
    if unix_map {
        print_map_stats(&source, "connected")?;
    }
    if hold_only {
        // The listener remains active: callbacks still validate and normalize
        // CPU pixels. Avoid read_event snapshots, FNV hashes and frame logging.
        let reason = tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(seconds)) => "timeout",
            result = tokio::signal::ctrl_c() => { result?; "signal" },
            _ = source.wait_closed() => "connection-closed",
        };
        if unix_map {
            print_map_stats(&source, "hold-ended-before-close")?;
            close_map_source(source, "hold-ended").await?;
        } else {
            source.close().await;
        }
        println!(
            "{}",
            serde_json::json!({"kind":"hold-ended","reason":reason})
        );
        ensure!(
            reason != "connection-closed",
            "QEMU D-Bus connection closed during hold"
        );
        return Ok(());
    }
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        let wait = Instant::now();
        let event = match timeout(
            deadline.saturating_duration_since(wait),
            source.read_event(),
        )
        .await
        {
            Err(_) => break,
            Ok(Err(error)) => {
                if unix_map {
                    print_map_stats(&source, "event-error-before-close")?;
                    close_map_source(source, "event-error").await?;
                }
                return Err(error);
            }
            Ok(Ok(event)) => event,
        };
        let frame = event.frame.as_ref().map(|frame| {
            let hash = frame
                .pixels
                .iter()
                .fold(0xcbf29ce484222325u64, |hash, byte| {
                    (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
                });
            serde_json::json!({"width":frame.width,"height":frame.height,"bytes":frame.pixels.len(),
                              "fnv1a64":format!("{hash:016x}")})
        });
        println!(
            "{}",
            serde_json::json!({"kind":"event","elapsed_ms":started.elapsed().as_secs_f64()*1000.0,
            "wait_ms":wait.elapsed().as_secs_f64()*1000.0,"revision":event.revision,
            "frame_revision":event.frame_revision,"disabled":event.disabled,"frame":frame,
            "source_bytes":event.stats.source_bytes,"scanouts":event.stats.scanouts,"updates":event.stats.updates,
            "normalization_ms_total":event.stats.normalization_ns as f64/1_000_000.0,
            "snapshot_ms":event.snapshot_ns as f64/1_000_000.0,
            "received_age_ms":event.received_at.map(|time|time.elapsed().as_secs_f64()*1000.0),
            "pixman_format":format!("0x{:08x}",event.stats.latest_format),"stride":event.stats.latest_stride})
        );
    }
    if unix_map {
        print_map_stats(&source, "deadline-before-close")?;
        close_map_source(source, "deadline").await?;
    } else {
        source.close().await;
    }
    Ok(())
}
