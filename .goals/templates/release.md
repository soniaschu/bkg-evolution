# Release Template

## Ziel
Release {{Version}}

## Hintergrund
{{Release Notes, Features, Fixes}}

## Erfolgskriterien
- [ ] Alle Tests grün
- [ ] Build erfolgreich
- [ ] Deployment erfolgreich
- [ ] Smoke Tests grün

## Einschränkungen
- Release Window: {{Time}}
- Rollback Plan: {{Ja}}

## Benötigte Analyse
- [ ] Changelog: ERFORDERT ANALYSE
- [ ] Breaking Changes: ERFORDERT ANALYSE

## Abhängigkeiten
- Dependencies updated

## Implementierungsplan
### Phase 1: Vorbereitung
- [ ] Version bump
- [ ] Changelog finalisieren
- [ ] Release Notes

### Phase 2: Build & Test
- [ ] CI Pipeline
- [ ] Alle Tests

### Phase 3: Deployment
- [ ] Staging Deploy
- [ ] Smoke Tests
- [ ] Production Deploy

### Phase 4: Post-Release
- [ ] Monitoring
- [ ] Kommunikation

## Validierung
- Version korrekt
- Artifacts verfügbar

## Tests
- [ ] Full Test Suite
- [ ] Smoke Tests

## Artefakte
- Release Binary
- Docker Images
- SBOM

## Dokumentation
- Release Notes
- Upgrade Guide

## Risiken
- Deployment Failure
- Breaking Changes

## Rollback
{{Rollback Procedure}}

## Abschlussbedingungen
- [ ] Release deployed
- [ ] Monitoring grün
- [ ] Team informiert

## Änderungsverlauf
- v1: Initial creation
