# HTTP brokerage contract

`rlean live --brokerage http --brokerage-url <URL>` separates private or
language-specific execution integrations from the rlean binary. The service
must expose JSON over HTTP. `--brokerage-account` is sent as the optional
`account_number` query or request field on every account-scoped operation.

## Required endpoints

| Method | Path | Response or request |
| --- | --- | --- |
| `GET` | `/brokerage` | `{"name":"Robinhood","brokerage_model":"robinhood"}` |
| `GET` | `/health` | Any `2xx` response means the process is alive. |
| `GET` | `/ready` | Any `2xx` response means authentication and broker I/O are ready. |
| `GET` | `/cash?account_number=...` | `[{"currency":"USD","amount":172.21}]` |
| `GET` | `/holdings?account_number=...` | Holdings described below. |
| `GET` | `/orders?account_number=...` | All brokerage orders required for crash reconciliation. |
| `GET` | `/orders/open?account_number=...` | Open brokerage orders only. |
| `POST` | `/order` | Submit an order and return an order response. |
| `POST` | `/cancel` | Cancel `{"order_id":"..."}` and return an order response. |

A holding is:

```json
{
  "symbol": "SPY",
  "quantity": 10,
  "average_buy_price": 500.25,
  "last_price": 501.10,
  "value": 5011.00
}
```

An order is:

```json
{
  "order_id": "broker-id",
  "symbol": "SPY",
  "side": "buy",
  "order_type": "market",
  "time_in_force": "gfd",
  "state": "filled",
  "quantity": 10,
  "filled_quantity": 10,
  "remaining_quantity": 0,
  "limit_price": null,
  "average_price": 501.10,
  "updated_at": "2026-08-05T14:31:00Z",
  "message": null
}
```

Order submission uses positive quantity plus an explicit `buy` or `sell`
action. Supported values in the initial contract are `market` or `limit` and
`day` or `gtc`:

```json
{
  "symbol": "SPY",
  "quantity": 10,
  "action": "buy",
  "order_type": "market",
  "time_in_force": "day",
  "limit_price": null,
  "account_number": "account-id"
}
```

Submission and cancellation return an order response with `success`, a human
message, and the normalized order object. The service must return a non-`2xx`
response for authentication failures, transport failures, malformed requests,
and unavailable broker state. rlean treats those as brokerage failures; it
never silently paper-fills them.
