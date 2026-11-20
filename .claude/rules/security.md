# Secrets and accounts

- Never print, log, store in fixtures, or commit passwords, 2FA codes, tokens, cookies,
  keyring items or anisette state.
- No live account-changing action (login, purchase, revoke) without the user asking for that run.
- Keep live request counts low; stop on HTTP 429.
