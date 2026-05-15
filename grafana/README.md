# Grafana dashboards

Dashboard JSON files for the operator + investor metrics surfaces on top
of `ligate-devnet-1`. Import each via Grafana → Dashboards → New → Import.

## Dashboards

| File | Use | Data sources |
|------|-----|--------------|
| `ligate-devnet-1-investor-metrics.json` | High-level "key numbers, growth, holders, network performance" surface. Shown to investors, partners, design-partner intros. | `Infinity` (api), `grafanacloud-prom` (chain Prometheus). |

The operator-focused dashboard (chain health, mempool, DA, RPC) lives in
the Grafana UI directly; if exported, drop it here too.

## One-time setup

The investor dashboard pulls api data via the
[Infinity plugin](https://grafana.com/grafana/plugins/yesoreyeram-infinity-datasource/)
(a generic JSON/REST data source). Set it up once:

1. **Install the plugin** (Grafana Cloud → Administration → Plugins →
   search "Infinity" → Install). Free; pre-installed on most stacks.
2. **Add the data source**: Grafana → Connections → Data sources → Add
   data source → search "Infinity". Configure:
   - **Name**: `Infinity (ligate-api)` — anything; the dashboard
     references it by `uid: ligate-api` below.
   - **URL** (under "URL, headers and params"): `https://api.ligate.io`
   - Leave auth / TLS empty (the api is public-read).
3. **Save the UID as `ligate-api`**: after saving the data source,
   open it from the data sources list, click the gear icon → "JSON
   Model", and set `"uid": "ligate-api"`. (Or edit the dashboard JSON
   to match whatever UID Grafana auto-assigned.)
4. **Confirm the Prometheus data source** UID. The dashboard
   references `grafanacloud-prom` for chain Prometheus metrics. If
   your stack's Prometheus data source has a different UID, find it in
   Grafana → Data sources → click the Prometheus source → look at the
   URL (`/datasources/edit/<uid>`) and either rename it or
   search-and-replace `grafanacloud-prom` in the dashboard JSON before
   import.

## Importing the investor dashboard

```
Grafana → Dashboards → New → Import → Upload JSON file
→ select ligate-devnet-1-investor-metrics.json
→ confirm data source mappings → Import.
```

Permalink-stable: the dashboard's `uid` is `ligate-devnet-1-investor`,
so future updates re-import cleanly onto the same dashboard.

## Panel inventory

The dashboard is organised into four rows:

### Key numbers (top row)

Six `stat` panels driven by `/v1/stats/totals` and
`/v1/stats/active-addresses?window=24h`:

- Total blocks
- Total txs (success)
- Total wallets
- Active wallets (24h)
- Schemas registered
- Attestations submitted

### Growth

Two timeseries panels:

- New wallets per day (last 30d): bar chart from
  `/v1/stats/new-wallets-daily?days=30`.
- Tx rate per day, by kind (last 30d): stacked bars from
  `/v1/stats/tx-rate-daily?days=30`. Stacks by `kind` (bank.transfer,
  attestation.submit_attestation, etc.) so the legend tells the story
  of how usage is distributed.

### Holder distribution

One table:

- Top 10 LGT holders: live-queried from chain via
  `/v1/stats/top-holders?n=10`. Refreshes every 30s (api stats cache
  TTL). Pre-mainnet this iterates every indexed address and queries
  the bank module per address; future indexer migration adds a
  `balance_nano` column to skip the chain round-trips.

### Network performance

Three Prometheus-driven panels:

- Mean block time (5m): `1 / rate(ligate_block_height[5m])`. Healthy
  on Mocha = ~12s.
- Sequencer uptime (24h): `avg_over_time(up{job=~".*ligate.*"}[24h])`.
  >99.9% is the bar.
- RPC request latency: p50/p95/p99 via
  `histogram_quantile(...) on ligate_rpc_request_duration_seconds_bucket`.

## Adding panels

The `/v1/stats/*` endpoints are documented in
[`crates/api/src/stats.rs`](../crates/api/src/stats.rs). Pattern for a
new stat panel:

```
{
  "type": "stat",
  "datasource": { "type": "yesoreyeram-infinity-datasource", "uid": "ligate-api" },
  "targets": [{
    "type": "json",
    "source": "url",
    "format": "table",
    "url": "/v1/stats/<endpoint>",
    "url_options": { "method": "GET" },
    "columns": [{ "selector": "<json-path>", "text": "...", "type": "number" }]
  }]
}
```

For Prometheus panels, use the existing chain metric names:
`ligate_block_height`, `ligate_mempool_depth`, `ligate_state_db_size_bytes`,
`ligate_rpc_request_duration_seconds`, `ligate_rpc_requests_total`,
`ligate_da_submission_failures_total`, `ligate_da_finalization_latency_seconds`,
`ligate_attestor_sets_registered_total`, `ligate_schemas_registered_total`,
`ligate_attestations_submitted_total`, `ligate_attestations_rejected_total`.
