# Pre-compiled EA

Drop the compiled `CascadaBridge.ex4` here. MT4 auto-compiles the `.mq4`
source Cascada installs into `MQL4/Experts/`, so this file is optional —
keep it checked in so users running a terminal without MetaEditor (headless
Wine setups) can copy the binary manually.

## How to build

1. Open MetaEditor 4
2. Open `ea/mt4/CascadaBridge.mq4`
3. Press F7 (Compile) — MetaEditor produces `CascadaBridge.ex4` next to the source
4. Copy that `.ex4` into this folder, commit

The Rust installer (`commands/install_mt.rs`) writes the `.mq4` source into
every discovered terminal; MT4 recompiles on next launch. This folder is
documentation/fallback only — nothing in the build pipeline reads it today.
