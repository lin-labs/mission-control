use mission_control::mc_data::surface_kind::{detect, detect_all};

fn main() {
    let ttys = vec!["ttys001", "ttys006", "ttys022", "ttys027"];
    let t0 = std::time::Instant::now();
    let map = detect_all(&ttys.iter().map(|s| *s).collect::<Vec<_>>());
    let batched = t0.elapsed();
    println!("=== detect_all ({} ttys, {:?}) ===", ttys.len(), batched);
    for tty in &ttys {
        println!("  {} -> {:?}", tty, map.get(*tty));
    }

    let t1 = std::time::Instant::now();
    for tty in &ttys {
        let _k = detect(tty);
    }
    let sequential = t1.elapsed();
    println!("=== sequential detect x{} took {:?} ===", ttys.len(), sequential);
    println!("speedup: {:.1}x", sequential.as_secs_f64() / batched.as_secs_f64());
}
