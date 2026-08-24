# Computer Use Command Reference

Model-facing contracts stay small (`computer.observe`, `computer.act`, `browser.act`, `mobile.act`); the CLI exposes ergonomic subcommands.

```text
rapid computer observe [--surface ...]
rapid computer move --x X --y Y
rapid computer click --target <semantic|x,y> [--button left] [--count 1]
rapid computer double-click --target ...
rapid computer down|up --button ...
rapid computer drag --from ... --to ... [--duration-ms N]
rapid computer scroll --dx N --dy N [--target ...]
rapid computer type --text ... | --secret <handle>
rapid computer key <key>
rapid computer chord <k1+k2+...>
rapid computer wait --condition <...> --timeout ...
rapid computer screenshot [--region x,y,w,h]
rapid computer cursor
rapid computer app list|activate|launch|quit
rapid computer window list|focus|move|resize
rapid computer record start|stop
rapid computer take-control|release-control

rapid browser open|navigate|back|forward|reload|tabs
rapid browser query|read-page|get-text|click|type|select|form
rapid browser console|network|screenshot|viewport
rapid browser js --expr ...       # policy gated
rapid browser upload|download     # policy gated

rapid mobile devices|boot|reset|install|launch|observe|tap|swipe|type|key|logs
```

Every state-changing action includes/derives an Observation generation. CLI coordinate actions require explicit current surface; stale generations reject. Secret typing resolves at executor boundary and is not echoed in normal logs.
