# Tasks: Photo albums

**Input**: Design documents from `/specs/001-photo-albums/`
**Prerequisites**: plan.md, research.md, data-model.md

## Phase 3.1: Setup
- [ ] T001 Create project structure per implementation plan
- [x] T002 [P] Configure linting in `.eslintrc.json`

## Phase 3.2: Tests First (TDD)
- [ ] T003 [P] [US1] Contract test POST /api/albums in tests/contract/test_albums_post.py
- [ ] T004 [P] Integration test album creation in tests/integration/test_create_album.py

## Phase 3.3: Core Implementation
- [ ] T005 [P] [US1] Album model in src/models/album.py
- [ ] T006 [US1] AlbumService CRUD in src/services/album_service.py
  - Uses `AlbumRepository` from src/models/album.py
- [ ] T007 POST /api/albums endpoint in src/api/albums.py

Decision: albums are flat, never nested.
