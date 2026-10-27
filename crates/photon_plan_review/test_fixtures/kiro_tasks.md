# Implementation Plan

- [ ] 1. Set up project structure and core interfaces
  - Create directories for models and services
  - Define the `UserStore` interface in `src/types.ts`
  - _Requirements: 1.1_

- [ ] 2. Implement data models
- [ ] 2.1 Create the User model
  - Write the `User` class with validation in `src/models/user.ts`
  - Write unit tests in `src/models/user.test.ts`
  - _Requirements: 1.2, 2.1_

- [x] 3. Wire the API
  - Add `createUser()` to `src/api/users.ts`
  - **Ruling:** emails are stored lower-cased
  - _Requirements: 3.1_
