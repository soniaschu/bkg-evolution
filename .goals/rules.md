# BKG Goal Rules

## Core Principles

1. **Erst analysieren, dann implementieren.** - Never start implementation without complete analysis.
2. **Keine Annahmen treffen.** - All unknowns must be marked as "ERFORDERT ANALYSE".
3. **Keine APIs erfinden.** - Use existing APIs, don't create fictional ones.
4. **Keine Dateien überschreiben ohne Sicherung.** - Always backup before modifying.
5. **Keine Erfolgsmeldung ohne erfolgreiche Tests.** - Tests must pass before claiming completion.
6. **Jede erledigte Aufgabe benötigt Nachweise.** - Evidence required for every completed task.
7. **Jede Implementierung benötigt Validierung.** - Validation step mandatory.
8. **Architektur muss erhalten bleiben.** - Don't break existing architecture.
9. **Keine TODOs im fertigen Goal.** - All TODOs must be resolved.
10. **Keine Platzhalter.** - Use "ERFORDERT ANALYSE" for unknowns.
11. **Immer reproduzierbare Schritte.** - Every step must be reproducible.
12. **Nur ein aktives Goal gleichzeitig.** - Single active goal constraint.
13. **Alte Goals niemals automatisch löschen.** - Archive instead of delete.
14. **Archiv niemals verändern.** - Archive is immutable.
15. **Zeitstempel protokollieren.** - All changes timestamped.

## Forbidden Terms

The following terms are NOT allowed in any goal section:
- TODO
- TBD
- Vielleicht
- Später
- Unknown
- Placeholder
- Dummy
- Mock

Exception: "ERFORDERT ANALYSE" is allowed and encouraged for unknown information.

## Validation Requirements

Every goal MUST have:
- At least one success criterion (checkable)
- At least one test defined
- Rollback plan
- All required sections completed
- No forbidden terms
- Version history maintained
