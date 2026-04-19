# Pre-compiled cBot

Drop a compiled `CascadaBridge.algo` file here. When present, the installer
copies it directly into `Sources/Robots/` — the user no longer has to click
*Build* in cTrader Automate, just refresh + attach to chart.

## How to build

1. Open cTrader → Automate
2. Add the `CascadaBridge.cs` source (already installed by Cascada)
3. Click *Build* — cTrader produces `~/cAlgo/Sources/Robots/CascadaBridge/bin/Release/CascadaBridge.algo`
4. Copy that file into this folder, commit.

At build time, Cascada embeds this file via `include_bytes!` and prefers it
over the `.cs` source.
