# Operator dashboard: panel replacements

Patches for the existing operator dashboard's "Chain activity" row.
The original "Schemas registered" and "Attestor sets registered"
panels query Prometheus counters that **drift on every node restart**
(see `crates/modules/attestation/src/metrics.rs` in the chain repo for
the full explanation — the counters re-fire during STF replay at boot,
so a single registered schema can show as 2, 3, ... after subsequent
restarts).

Replacement panels below query the api's `/v1/stats/totals` instead,
which counts the rows actually present in the indexer DB. That's the
same number an explorer or block scanner would compute. Once the
chain bumps the SDK fork rev and `ligate-api#35` (indexer dropping
attestation txs) is fixed, these numbers match chain state exactly.

## Prereqs

The api Infinity data source must already exist in your Grafana stack
with UID `ligate-api` — same as the investor dashboard. If you haven't
done that yet, follow steps 1-3 of [`README.md`](./README.md).

## How to apply

For each of the two panels:

1. Open the operator dashboard in Grafana.
2. Click the panel's title → **More...** → **Inspect** → **Panel JSON**.
3. Replace the panel JSON with the corresponding block below.
4. Apply → Save dashboard.

Alternative: use Grafana's "Edit panel" UI, switch the data source to
`Infinity (ligate-api)`, and configure the query manually as
`json` / `url` / `/v1/stats/totals` with the relevant `selector`.

## Replacement: "Schemas registered"

Selects `schemas` from `/v1/stats/totals`. Replaces the
`ligate_schemas_registered_total` Prometheus query.

```json
{
  "type": "stat",
  "title": "Schemas registered",
  "description": "Count of registered attestation schemas, from the api indexer (single source of truth). Was previously a Prometheus counter (`ligate_schemas_registered_total`) which double-counted on every chain-node restart due to STF replay.",
  "datasource": { "type": "yesoreyeram-infinity-datasource", "uid": "ligate-api" },
  "fieldConfig": {
    "defaults": {
      "color": { "mode": "thresholds" },
      "thresholds": { "mode": "absolute", "steps": [{ "color": "green", "value": null }] },
      "unit": "short"
    }
  },
  "options": {
    "colorMode": "value",
    "graphMode": "area",
    "reduceOptions": { "calcs": ["lastNotNull"], "fields": "", "values": false },
    "textMode": "auto"
  },
  "targets": [
    {
      "datasource": { "type": "yesoreyeram-infinity-datasource", "uid": "ligate-api" },
      "refId": "A",
      "type": "json",
      "source": "url",
      "format": "table",
      "url": "/v1/stats/totals",
      "url_options": { "method": "GET" },
      "columns": [{ "selector": "schemas", "text": "Schemas", "type": "number" }]
    }
  ]
}
```

## Replacement: "Attestor sets registered"

Same shape, different selector.

```json
{
  "type": "stat",
  "title": "Attestor sets registered",
  "description": "Count of registered attestor sets, from the api indexer (single source of truth). Was previously a Prometheus counter which double-counted on chain-node restart.",
  "datasource": { "type": "yesoreyeram-infinity-datasource", "uid": "ligate-api" },
  "fieldConfig": {
    "defaults": {
      "color": { "mode": "thresholds" },
      "thresholds": { "mode": "absolute", "steps": [{ "color": "green", "value": null }] },
      "unit": "short"
    }
  },
  "options": {
    "colorMode": "value",
    "graphMode": "area",
    "reduceOptions": { "calcs": ["lastNotNull"], "fields": "", "values": false },
    "textMode": "auto"
  },
  "targets": [
    {
      "datasource": { "type": "yesoreyeram-infinity-datasource", "uid": "ligate-api" },
      "refId": "A",
      "type": "json",
      "source": "url",
      "format": "table",
      "url": "/v1/stats/totals",
      "url_options": { "method": "GET" },
      "columns": [{ "selector": "attestor_sets", "text": "Attestor sets", "type": "number" }]
    }
  ]
}
```

## Bonus: "Attestations submitted" (rate)

The current `attestations / sec` panel uses
`rate(ligate_attestations_submitted_total[5m])`, which is fine for
event-rate questions — `rate()` handles counter resets gracefully, so
the per-process drift doesn't matter here. Leave that one alone.

If you also want a "total attestations" stat panel that matches
state, add a third panel using the same shape as above with
`"selector": "attestations"`.

## Why we can't auto-import this

The operator dashboard's full JSON lives only in Grafana Cloud and
wasn't checked in. Once the fix above is applied, export the
dashboard JSON via Grafana → Dashboards → Share → Export → "Save to
file" and drop the result in this directory so future edits are
reproducible (the same path the investor dashboard already uses).
