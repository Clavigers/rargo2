//! Prints a plan file in readable form: `dump <plan> [unit-substring]`.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bytes = std::fs::read(&args[1]).expect("read plan");
    let plan: rargo_plan::Plan = rargo_plan::decode(&bytes).expect("decode plan");
    println!(
        "plan v{} ws={} cwd={} target={} host={} toolchain={:?} units={} roots={:?} steps={}",
        plan.version,
        plan.ws_root,
        plan.cwd,
        plan.target_dir,
        plan.host,
        plan.toolchain,
        plan.units.len(),
        plan.roots,
        plan.steps.len()
    );
    println!("finished: {:?} fmt={:?} verbosity={:?}", plan.finished, plan.message_format, plan.verbosity);
    let filter = args.get(2);
    for (i, u) in plan.units.iter().enumerate() {
        if let Some(f) = filter {
            if !u.desc.contains(f.as_str()) && !u.pkg.contains(f.as_str()) {
                continue;
            }
            println!("--- unit {i}: {:?} {} [{}] rmeta_early={} deps={:?}", u.kind, u.desc, u.pkg, u.rmeta_early, u.deps);
            if let Some(c) = &u.cmd {
                println!("  cwd: {:?}\n  program: {}\n  args: {}", c.cwd, c.program, c.args.join(" "));
                for (k, v) in &c.env {
                    println!("  env {k}={v:?}");
                }
            }
            for o in &u.outputs {
                println!("  out {} -> {:?} ({})", o.path, o.hardlink, o.flavor);
            }
            println!("  script: {:?}", u.script.as_ref().map(|s| (&s.meta, &s.out_dir)));
        } else {
            println!("{i:4} {:?} {:?} {} deps={}", u.kind, u.verb, u.desc, u.deps.len());
        }
    }
    for s in &plan.steps {
        match s {
            rargo_plan::Step::Status { verb, msg, when } => println!("step status {verb} {msg} ({when:?})"),
            rargo_plan::Step::Exec { kind, unit, cmd, rerun, .. } => {
                println!("step exec {kind:?} unit={unit} rerun=`{rerun}`\n   {} {}", cmd.program, cmd.args.join(" "))
            }
            rargo_plan::Step::Run { display, .. } => println!("step run {display}"),
            rargo_plan::Step::Open { cmd } => println!("step open {} {}", cmd.program, cmd.args.join(" ")),
        }
    }
}
