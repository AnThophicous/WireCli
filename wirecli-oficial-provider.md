# Wire CLI Provider Contract

This document defines the Wire-native provider contract used by Wire CLI.

Wire is the control plane. The CLI never talks to OpenRouter directly.
The CLI talks to Wire endpoints, and Wire translates the request to the upstream provider.

## Core goals

- Keep the CLI protocol stable and Wire-owned.
- Authenticate with PKCE and a user session token.
- Return account, provider and model visibility after auth.
- Stream completion traffic through a Wire SSE format.
- Support stop by `requestId`.

## Authentication flow

### 1. Create auth request

`POST /api/v1/auth/wireapp/authflux`

Request body:

```json
{
  "appId": "wire-cli",
  "appName": "Wire CLI",
  "appLogoUrl": "https://example.com/logo.png",
  "codeChallenge": "BASE64URL_SHA256",
  "codeChallengeMethod": "S256",
  "requestedScopes": "account:read providers:read models:read",
  "redirectUri": "wirecli://auth/callback"
}
```

Response:

```json
{
  "ok": true,
  "requestId": "req_...",
  "verificationUrl": "/wireapp/authorize/req_...",
  "pollUrl": "/api/v1/auth/wireapp/authflux/req_...",
  "expiresAt": "2026-06-02T19:00:00.000Z"
}
```

### 2. Poll auth request

`GET /api/v1/auth/wireapp/authflux/<requestId>`

Returns pending, approved, expired, or exchanged state.

### 3. Approve in browser

`POST /api/v1/auth/wireapp/authflux/<requestId>/approve`

Used by the signed-in browser session only.

### 4. Exchange PKCE code

`POST /api/v1/auth/wireapp/authflux/<requestId>/exchange`

Request body:

```json
{
  "authorizationCode": "auth_code_from_approve",
  "codeVerifier": "original_pkce_verifier"
}
```

Response:

```json
{
  "ok": true,
  "token": "wire-session-token",
  "accountId": "uuid-of-user",
  "providers": [],
  "models": [],
  "billing": {
    "plan": "free",
    "creditsUsd": 0,
    "weeklyUsageUsd": 0,
    "monthlyQuoteUsd": 0,
    "payAsYouGoValidationUsd": 0,
    "weeklyFeeUsd": 0
  },
  "relayKey": null,
  "account": {}
}
```

## Account snapshot

### `GET /api/v1/wireapp/account`

Returns the full account snapshot for the CLI.

Important fields:

- `accountId`
- `user.id`
- `user.fullName`
- `user.displayName`
- `user.avatarUrl`
- `user.billingPlan`
- `user.themePreference`
- `providers[]`
- `models[]`
- `connectedProviderAccounts[]`
- `billing`

### `GET /api/v1/wireapp/providers`

Returns only provider availability.

Response:

```json
{
  "ok": true,
  "accountId": "uuid-of-user",
  "providers": [
    {
      "id": "openrouter",
      "label": "OpenRouter",
      "available": true,
      "source": "connected",
      "models": ["openrouter/anthropic/claude-opus-4.8"]
    }
  ],
  "billing": {
    "plan": "free",
    "creditsUsd": 0,
    "weeklyUsageUsd": 0,
    "monthlyQuoteUsd": 0,
    "payAsYouGoValidationUsd": 0,
    "weeklyFeeUsd": 0
  }
}
```

### `GET /api/v1/wireapp/models`

Returns only model availability.

Response:

```json
{
  "ok": true,
  "accountId": "uuid-of-user",
  "models": [
    {
      "id": "claude-opus-4.8",
      "provider": "anthropic",
      "label": "Anthropic / claude-opus-4.8",
      "available": true,
      "source": "connected"
    }
  ]
}
```

### `GET /api/demo/health`

Simple liveness endpoint for local wiring.

Response:

```json
{
  "ok": true,
  "route": "GET /api/demo/health",
  "service": "wireai-api",
  "status": "healthy",
  "accent": "#111111",
  "message": "local backend ready",
  "uptime_ms": 1234
}
```

### `GET /api/demo/models`

Local demo model registry used by the landing/demo surfaces.

Response:

```json
{
  "ok": true,
  "route": "GET /api/demo/models",
  "accent": "#222222",
  "default_model": "wireai/auto",
  "models": [
    {
      "id": "wireai/auto",
      "name": "WireAI Auto",
      "tier": "smart-routing",
      "accent": "#171717"
    }
  ]
}
```

### `GET /api/integrations/openrouter/start`

Starts the OpenRouter PKCE flow and redirects to OpenRouter.

### `GET /api/integrations/openrouter/callback`

Consumes the OAuth callback, stores the upstream key, and redirects back to the dashboard.

Response behavior:

- success: `302` back to the redirect target
- invalid callback: `302 /dashboard?error=invalid_openrouter_callback`
- missing verifier: `302 /dashboard?error=invalid_openrouter_verifier`
- exchange failure: `302 /dashboard?error=openrouter_callback_failed`

### `GET /api/dashboard/overview`

Dashboard payload for the product UI.

Important fields:

- `user.emailVerified`
- `wireapp.providers`
- `integrations.openRouter.connected`
- `integrations.openRouter.models`
- `apiKeys[]`
- `usage.recent[]`

### `POST /api/dashboard/providers`

Stores a provider secret key and marks that provider ready.

Response:

```json
{
  "ok": true,
  "provider": {
    "id": "uuid",
    "provider": "anthropic",
    "displayName": "Claude",
    "connectedAt": "2026-06-02T19:00:00.000Z",
    "updatedAt": "2026-06-02T19:00:00.000Z"
  }
}
```

### `DELETE /api/dashboard/providers?provider=<id>`

Revokes a stored provider key.

Response:

```json
{
  "ok": true,
  "revokedId": "uuid",
  "provider": "anthropic"
}
```

### `DELETE /api/dashboard/api-keys/<keyId>`

Revokes a Wire API key.

Response:

```json
{
  "ok": true,
  "revokedId": "uuid"
}
```

## Native agent request

### `POST /api/v1/agents/request/<requestId>`

This is the Wire-native completion endpoint for Wire CLI.

The body must include the Wire session token, account id and request id.

Request body:

```json
{
  "requestId": "req_123",
  "accountId": "uuid-of-user",
  "token": "wire-session-token",
  "providerId": "openrouter",
  "model": "openrouter/anthropic/claude-opus-4.8",
  "messages": [
    { "role": "system", "content": "You are Wire CLI." },
    { "role": "user", "content": "Summarize this repo." }
  ],
  "tools": []
}
```

Validation rules:

- `requestId` in the body must match the path parameter.
- `token` must resolve to a live Wire session.
- `accountId` must match the resolved user id.
- `providerId` must be connected for the account.
- `model` must be accepted by the upstream provider.

### SSE format

Wire uses Server-Sent Events with explicit event names.

Each frame has:

```text
event: request.started
data: {...}

event: thinking.started
data: {...}

event: thinking.delta
data: {...}

event: assistant.delta
data: {...}

event: tool.call
data: {...}

event: choice.finish
data: {...}

event: usage
data: {...}

event: request.completed
data: {...}
```

### Event payloads

#### `request.started`

```json
{
  "requestId": "req_123",
  "accountId": "uuid-of-user",
  "providerId": "openrouter",
  "model": "openrouter/anthropic/claude-opus-4.8"
}
```

#### `thinking.started`

```json
{
  "requestId": "req_123",
  "model": "openrouter/anthropic/claude-opus-4.8"
}
```

#### `thinking.delta`

```json
{
  "requestId": "req_123",
  "delta": "reasoning text"
}
```

#### `assistant.delta`

```json
{
  "requestId": "req_123",
  "delta": "assistant text"
}
```

#### `tool.call`

```json
{
  "requestId": "req_123",
  "id": "toolcall_1",
  "type": "function",
  "name": "search_files",
  "arguments": "{\"query\":\"...\"}"
}
```

#### `usage`

```json
{
  "requestId": "req_123",
  "promptTokens": 120,
  "completionTokens": 380,
  "costUsd": 0.0042
}
```

#### `request.completed`

```json
{
  "requestId": "req_123",
  "model": "openrouter/anthropic/claude-opus-4.8"
}
```

#### `request.stopped`

```json
{
  "requestId": "req_123"
}
```

#### `error`

```json
{
  "requestId": "req_123",
  "message": "Upstream provider rejected the request."
}
```

## Stop request

### `POST /api/v1/agents/stop/<requestId>`

Stops the active Wire agent request for that `requestId`.

Response:

```json
{
  "ok": true,
  "requestId": "req_123",
  "stopped": true
}
```

## Provider behavior

- `openrouter` is the initial upstream adapter.
- Connected provider keys are stored server-side.
- The dashboard marks configured providers as `Ready`.
- Paid plans unlock the provider surface.
- Free plans keep provider configuration locked until the user connects or upgrades.

## CLI expectations

The CLI should:

- store the session token returned by auth exchange
- pass `token`, `accountId`, `requestId`, `providerId`, `model`, `messages` and `tools` in the body
- read SSE event names directly
- abort the HTTP connection when the user asks to stop
- call `/api/v1/agents/stop/<requestId>` when it needs server-side abort
