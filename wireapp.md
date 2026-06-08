# Wire App / Wire CLI contract

Base URL:

- browser/app routes: `http://localhost:3000/v1/...`
- Next.js implementation lives under `/api/v1/...`
- this project rewrites `/v1/:path*` to `/api/v1/:path*`

Authentication model:

- the CLI never ships a client secret
- auth is PKCE-based
- the browser user must be signed in to Wire before approving a CLI request
- the approved request returns a bearer session token
- that token is the same session-token shape Wire already issues, but it can now be sent as `Authorization: Bearer ...` as well as a cookie

## 1) Start auth flow

`POST /v1/auth/wireapp/authflux`

Request body:

```json
{
  "appId": "wire-cli",
  "appName": "Wire CLI",
  "appLogoUrl": "https://... or data:image/png;base64,...",
  "codeChallenge": "base64url(SHA256(code_verifier))",
  "codeChallengeMethod": "S256",
  "requestedScopes": "account:read providers:read models:read",
  "redirectUri": "http://127.0.0.1:port/callback"
}
```

Notes:

- `appId` is required and should be stable for the client
- `appName` is displayed to the user during approval
- `appLogoUrl` can be a normal URL or a data URL
- `codeChallengeMethod` defaults to `S256`
- `requestedScopes` is informational in the current implementation
- `redirectUri` is stored for the auth request and can be echoed by the client

Response:

```json
{
  "ok": true,
  "request": {
    "requestId": "req_...",
    "appId": "wire-cli",
    "appName": "Wire CLI",
    "appLogoUrl": null,
    "codeChallenge": "...",
    "codeChallengeMethod": "S256",
    "requestedScopes": "account:read providers:read models:read",
    "redirectUri": "http://127.0.0.1:port/callback",
    "status": "pending",
    "expiresAt": "2026-06-02T...",
    "createdAt": "2026-06-02T..."
  }
}
```

The same response also implies:

- approval page: `/wireapp/authorize/<requestId>`
- poll endpoint: `/v1/auth/wireapp/authflux/<requestId>`

## 2) Poll auth flow

`GET /v1/auth/wireapp/authflux/<requestId>`

Response while pending:

```json
{
  "ok": true,
  "request": {
    "requestId": "req_...",
    "appId": "wire-cli",
    "appName": "Wire CLI",
    "appLogoUrl": null,
    "requestedScopes": "account:read providers:read models:read",
    "status": "pending",
    "expiresAt": "2026-06-02T..."
  }
}
```

Response after approval:

```json
{
  "ok": true,
  "request": {
    "requestId": "req_...",
    "appId": "wire-cli",
    "appName": "Wire CLI",
    "appLogoUrl": null,
    "requestedScopes": "account:read providers:read models:read",
    "status": "approved",
    "expiresAt": "2026-06-02T...",
    "approvedAt": "2026-06-02T...",
    "authorizationCode": "one-time-code"
  }
}
```

## 3) Approve in browser

`POST /v1/auth/wireapp/authflux/<requestId>/approve`

Requirements:

- browser session must already be authenticated
- same-origin request

Response:

```json
{
  "ok": true,
  "request": {
    "requestId": "req_...",
    "status": "approved"
  },
  "authorizationCode": "one-time-code"
}
```

The approval page is:

- `/wireapp/authorize/<requestId>`

## 4) Exchange code + verifier

`POST /v1/auth/wireapp/authflux/<requestId>/exchange`

Request body:

```json
{
  "authorizationCode": "one-time-code",
  "codeVerifier": "original PKCE verifier"
}
```

Response:

```json
{
  "ok": true,
  "token": "wire-session-token",
  "accountId": "user-uuid",
  "providers": [],
  "models": [],
  "billing": {
    "plan": "monthly",
    "creditsUsd": 50,
    "weeklyUsageUsd": 0,
    "monthlyQuoteUsd": 54,
    "payAsYouGoValidationUsd": 0,
    "weeklyFeeUsd": 0
  },
  "relayKey": null,
  "account": {
    "accountId": "user-uuid",
    "user": {
      "id": "user-uuid",
      "fullName": "Wire User",
      "displayName": "Wire",
      "avatarUrl": null,
      "billingPlan": "monthly",
      "themePreference": "system"
    },
    "providers": [],
    "models": [],
    "connectedProviderAccounts": [],
    "billing": {}
  }
}
```

Implementation notes:

- `authorizationCode` is one-time
- `codeVerifier` must satisfy the stored PKCE challenge
- the returned `token` is a Wire session token and can be used as `Authorization: Bearer ...`
- the same token also works as the browser cookie session token shape

## 5) Account snapshot

`GET /v1/wireapp/account`

Returns the signed-in account profile, provider availability, model availability and billing summary.

## 6) Provider inventory

`GET /v1/wireapp/providers`

Returns the provider list with availability sourced from:

- the user billing plan
- connected provider accounts on the Wire account

`GET /v1/wireapp/models`

Optional query:

- `?provider=openai`

Returns the filtered model list for the signed-in account.

## Billing model

Current billing inputs:

- `free`
- `payg`
- `monthly`

Pay as you go:

- the validation deposit is `3 USD`
- weekly billing is usage plus `0.50 USD`
- if weekly usage is `0`, nothing is billed that week

Monthly:

- user selects a credit amount
- backend computes the quoted amount and platform fee before creating the payment preference

Payment checkout:

- `POST /api/billing/checkout`
- accepts `billingPlan` and, for monthly, `creditsUsd`

## Wire CLI behavior expected

1. Generate `code_verifier`
2. Derive `code_challenge = base64url(SHA256(code_verifier))`
3. POST the auth start payload
4. Open `/wireapp/authorize/<requestId>` in the browser
5. Poll `/v1/auth/wireapp/authflux/<requestId>`
6. Read `authorizationCode` after approval
7. POST exchange with `authorizationCode` and `codeVerifier`
8. Store returned bearer token securely in the CLI profile

This file is intentionally outside the website docs tree so it can be copied into the Wire CLI repo as implementation context.
