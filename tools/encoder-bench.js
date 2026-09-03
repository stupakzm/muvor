// §5.4c's measurement: what the capture encoder actually costs.
//
// Produced the table that decided D17 — that capture stays full-resolution
// PNG at ~100 ms and none of the speed levers are taken. Re-run it against
// a frame from `muvor capture --out` if that decision is ever re-opened:
//
//   muvor capture --out /tmp/frame.png
//   gjs tools/encoder-bench.js /tmp/frame.png
//
// What it found, at 1920x1080 on a dark-terminal frame: compression=9 is a
// trap (3.1x the time for 2.5% fewer bytes), JPEG q90 is 11x faster than
// PNG, and encode is linear in pixels. None of it is reachable from GJS —
// `screenshot_area` hardcodes PNG with no options pass-through — which is
// why the reachable levers are `composite_to_stream`'s scale and rectangle.

imports.gi.versions.GdkPixbuf = '2.0';
const { GdkPixbuf, Gio, GLib } = imports.gi;

const src = GdkPixbuf.Pixbuf.new_from_file(ARGV[0]);
print(`frame           ${src.get_width()}x${src.get_height()} ${src.get_has_alpha() ? 'RGBA' : 'RGB'}`);

function bench(label, pb, fmt, keys, vals, n) {
    const t = [];
    let size = 0;
    for (let i = 0; i < n; i++) {
        const st = Gio.MemoryOutputStream.new_resizable();
        const t0 = GLib.get_monotonic_time();
        pb.save_to_streamv(st, fmt, keys, vals, null);
        t.push((GLib.get_monotonic_time() - t0) / 1000);
        st.close(null);
        size = st.steal_as_bytes().get_size();
    }
    t.sort((a, b) => a - b);
    const mean = t.reduce((a, b) => a + b, 0) / t.length;
    print(`${label.padEnd(30)} min ${t[0].toFixed(1).padStart(6)}  mean ${mean.toFixed(1).padStart(6)}  max ${t[t.length-1].toFixed(1).padStart(6)} ms   ${(size/1048576).toFixed(3)} MB`);
}

const N = 6;
bench('png default', src, 'png', [], [], N);
for (const c of ['0', '1', '3', '6', '9'])
    bench(`png compression=${c}`, src, 'png', ['compression'], [c], N);
for (const q of ['90', '75'])
    bench(`jpeg quality=${q}`, src, 'jpeg', ['quality'], [q], N);

const half = src.scale_simple(src.get_width() / 2, src.get_height() / 2, GdkPixbuf.InterpType.BILINEAR);
bench('png 960x540 default', half, 'png', [], [], N);
bench('png 960x540 compression=1', half, 'png', ['compression'], ['1'], N);

const t0 = GLib.get_monotonic_time();
src.scale_simple(src.get_width() / 2, src.get_height() / 2, GdkPixbuf.InterpType.BILINEAR);
print(`(the downscale itself costs ${((GLib.get_monotonic_time() - t0)/1000).toFixed(1)} ms)`);
