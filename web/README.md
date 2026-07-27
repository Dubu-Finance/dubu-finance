# web

One file, no build step, no dependencies.

```
cd web && python3 -m http.server 8777
open http://localhost:8777
```

It reads the chain directly from the browser — every endpoint it needs sends
`access-control-allow-origin: *`, checked before the page was written — and takes its reference
price from Binance's public `bookTicker` stream, which needs no key. Nothing is served by us.

What it shows, per market: the reference price the updater tracks, the pool's posted ladder and
how far it has drifted from that reference, and the same trade priced against both the prop AMM
and a UniswapV2 pair holding identical inventory.

The status bar is the thing to watch during a demo. `quote age` and `capacity` are how you see
whether the updater is alive: past `maxStaleSecs` the pool stops quoting by design, and a stopped
updater shows up here within a block rather than as a mysteriously empty table.

The four caveats on the comparison are printed on the page itself, under the tables, because the
ratio is the number most likely to be screenshotted without them.
