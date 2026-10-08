# Deployment Template

## Ziel
Deployment von {{Application}} nach {{Environment}}

## Hintergrund
{{Release Info, Changes}}

## Erfolgskriterien
- [ ] Deployment erfolgreich
- [ ] Health Checks grün
- [ ] Smoke Tests grün

## Einschränkungen
- Downtime: {{Max}}
- Rollback Time: {{Max}}

## Benötigte Analyse
- [ ] Infrastructure: ERFORDERT ANALYSE
- [ ] Config: ERFORDERT ANALYSE
- [ ] Secrets: ERFORDERT ANALYSE

## Abhängigkeiten
- CI/CD Pipeline
- Infrastructure Ready

## Implementierungsplan
### Phase 1: Vorbereitung
- [ ] Artifacts bereit
- [ ] Config prüfen
- [ ] Secrets setzen

### Phase 2: Deployment
- [ ] Deploy Staging
- [ ] Tests
- [ ] Deploy Production

### Phase 3: Validierung
- [ ] Health Checks
- [ ] Smoke Tests
- [ ] Monitoring

## Validierung
- Alle Services healthy
- Keine Errors in Logs

## Tests
- [ ] Smoke Tests
- [ ] Integration Tests

## Artefakte
- Deployment Logs
- Artifacts

## Dokumentation
- Runbook aktualisiert

## Risiken
- Config Drift
- Capacity Issues

## Rollback
{{Rollback Procedure}}

## Abschlussbedingungen
- [ ] Deployment grün
- [ ] Monitoring grün
- [ ] Team informiert

## Änderungsverlauf
- v1: Initial creation
