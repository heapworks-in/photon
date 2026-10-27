## 1. Implementation
- [ ] 1.1 Add the `two_factor_secret` column in `db/migrations/0042_two_factor.sql`
- [ ] 1.2 Add `verify_totp()` to `src/auth/totp.py`
- [ ] 1.3 Update the login flow in `src/auth/login.py`

## 2. Tests
- [ ] 2.1 Add `test_login_requires_totp` to `tests/test_login.py`
