use xbuild::{BuildArgs, BuildEnv, BuildTargetArgs, CargoArgs};

// cargo run
fn main() {

    let mut build_target = BuildTargetArgs::default();
    build_target.debug = true;
    let cargo = CargoArgs::default();
    let args = BuildArgs { build_target, cargo, verbose: true };
    let env = BuildEnv::new(args).unwrap();
    xbuild::command::build(&env).unwrap();
}
