# Continuum roadmap

Bron: [Continuum.md](./Continuum.md). Dit document is de uitvoerbare planning; `Continuum.md` blijft de volledige productvisie.

## Statuslegenda

- [x] Klaar en lokaal of op de testserver bewezen
- [ ] Nog te doen
- **Owner** is eindverantwoordelijk; samenwerken blijft toegestaan

## Teamverdeling

| Persoon | Hoofdverantwoordelijkheid |
| --- | --- |
| **Toshan** | Product, UX, Context Model, Main AI, context compiler, modelrouter en demo |
| **Arda** | Windows service, observers, opslag, permissions, security, installer en performance |
| **Samen** | Architectuurcontracten, integratietests, releases en gebruikersvalidatie |

Deze verdeling is het startpunt. Wissel taken als ervaring of beschikbaarheid daar aanleiding toe geeft, maar houd per taak precies één owner.

## Fase 0 — Fundament en productrichting

**Doel:** van de Kairo-donor een reproduceerbare, eerlijke Continuum-basis maken.

### Product en architectuur

- [x] **Toshan:** zes doelmockups vertalen naar de Continuum desktop-shell
- [x] **Toshan:** echte navigatie voor Home, Projects, Memory, Agents, Permissions, Timeline en Settings
- [x] **Toshan:** echte Handoff ↔ Context Compiler / Launch Pad-flow
- [x] **Samen:** mockdata duidelijk scheiden van live backendclaims
- [x] **Samen:** `CONTINUUM_ARCHITECTURE.md` vastleggen als technisch contract
- [x] **Samen:** donorgrenzen, migratievolgorde en v0.1-acceptatiedemo vastleggen
- [x] **Samen:** nieuwe `Continuum.md` uit GitHub opnemen als productbron

### Reproduceerbaarheid en kwaliteit

- [x] **Arda:** pnpm en Rust-toolchain pinnen
- [x] **Arda:** frozen dependency-install herstellen
- [x] **Arda:** CI-fouten niet langer verbergen met `continue-on-error`
- [x] **Arda:** CUDA opt-in maken zodat standaardbuilds draagbaar zijn
- [x] **Arda:** machine-specifieke Cargo-paden verwijderen
- [x] **Arda:** provenance- en licentie-audit documenteren
- [x] **Samen:** frontend typecheck, lint, format en productiebuild op testserver
- [x] **Samen:** docs-productiebuild op testserver
- [x] **Samen:** Rust fmt, light Clippy en 98 light tests op testserver
- [x] **Arda:** volledige Rust workspace Clippy + tests groen maken
- [ ] **Arda:** Tauri desktopbuild op Windows CI groen bewijzen
- [ ] **Samen:** gewijzigde blocking CI-workflow pushen en één volledig groene run bewijzen
- [x] **Samen:** alle zeven tabs plus Ctrl+K-flow headless op de testserver bewijzen
- [x] **Samen:** zes doelmockups op 1600×1000 zonder overflow of console-errors bewijzen

### Release-blockers uit provenance

- [x] **Arda:** beschadigde Apache-licentietekst onderzoeken en canoniek herstellen
- [x] **Arda:** Lessac-stem vervangen door de vanaf nul getrainde Norman-stem
- [x] **Arda:** Piper/eSpeak-archief op bestandsniveau auditen; 361 bestanden en geen meegeleverde `LICENSE`/`NOTICE`, dus bundling blijft geblokkeerd
- [x] **Arda:** volledige transitive CycloneDX-SBOM en pnpm-licentierapport genereren en archiveren

**Fase 0 klaar wanneer:** alle bovenstaande checks zijn afgevinkt, de blocking GitHub CI groen is en de mockupflows op de testserver zonder console-errors werken.

## Fase 1 — Observer en live context

**Resultaat:** Continuum weet wat actief is en kan de laatste werkzaamheden samenvatten.

- [ ] **Arda:** Windows background service + auto-start bouwen
- [ ] **Arda:** actieve app en venstertitel event-driven volgen
- [ ] **Arda:** filesystem watcher en Git-context bouwen
- [ ] **Arda:** screenshot change detector met resourcebudget bouwen
- [ ] **Arda:** append-only eventdatabase implementeren
- [ ] **Toshan:** projectdetectie en goal-confidence implementeren
- [ ] **Toshan:** live Context-pagina op echte events aansluiten
- [ ] **Samen:** privacyfilter vóór opslag en modelcalls testen

## Fase 2 — Geheugen

**Resultaat:** Continuum onthoudt projecten en beslissingen betrouwbaar over meerdere dagen.

- [ ] **Arda:** typed entities voor projects, sessions, tasks, decisions en blockers opslaan
- [ ] **Arda:** SQLite event store + migraties + back-up/restore bouwen
- [ ] **Toshan:** session summarization en memory candidates bouwen
- [ ] **Toshan:** confidence, conflict detection en bronverwijzingen toevoegen
- [ ] **Toshan:** semantisch zoeken en contextrelevantie bouwen
- [ ] **Arda:** Markdown-vault export/import bouwen
- [ ] **Toshan:** Memory review-UX en correctieflow bouwen
- [ ] **Samen:** memory precision- en retentietests uitvoeren

## Fase 3 — Twee-modellenarchitectuur

**Resultaat:** het Context Model verzamelt context; de Main AI voert zware redeneertaken uit.

- [ ] **Toshan:** lokale Context Model-runtime integreren
- [ ] **Toshan:** Main AI gateway met provider-adapters bouwen
- [ ] **Toshan:** versioned context package contract implementeren
- [ ] **Toshan:** modelrouter voor privacy, kosten en kwaliteit bouwen
- [ ] **Arda:** secrets lokaal en versleuteld beheren
- [ ] **Arda:** latency-, token- en resourcebudgetten afdwingen
- [ ] **Samen:** provider contracttests en context-pack evaluaties bouwen

## Fase 4 — “Ga door”

**Resultaat:** na een pauze kiest Continuum aantoonbaar de juiste onafgemaakte taak en stelt het juiste vervolg voor.

- [ ] **Toshan:** task-state machine en goal tracker bouwen
- [ ] **Toshan:** unfinished-task detector en continuation resolver bouwen
- [ ] **Toshan:** planner en verifier bouwen
- [ ] **Arda:** permission preflight vóór iedere toolcall afdwingen
- [ ] **Arda:** durable checkpoints en crash recovery bouwen
- [ ] **Samen:** restart- en continuation-evaluaties met echte projecten uitvoeren

## Fase 5 — Gecontroleerde acties

**Resultaat:** Continuum kan goedgekeurd werk uitvoeren met audit trail en rollback.

- [ ] **Arda:** capability-gebaseerd tool- en permissionsysteem bouwen
- [ ] **Arda:** file- en terminaltools sandboxen
- [ ] **Toshan:** browser-, IDE- en code-agent adapters bouwen
- [ ] **Arda:** append-only auditlog en emergency stop bouwen
- [ ] **Arda:** rollback voor bestanden, Git en databaseacties bouwen
- [ ] **Toshan:** approval previews en resultaatverificatie in de UI bouwen
- [ ] **Samen:** deny-, revoke-, rollback- en secret-redactiontests uitvoeren

## Fase 6 — Desktop control

**Resultaat:** Continuum kan apps zonder directe integratie veilig bedienen.

- [ ] **Arda:** Windows UI Automation adapter bouwen
- [ ] **Toshan:** browser-DOM adapter bouwen
- [ ] **Arda:** visuele fallback en mouse/keyboard control bouwen
- [ ] **Arda:** focus-, coordinate- en stale-screen guards bouwen
- [ ] **Toshan:** veilige bevestigings- en previewflow ontwerpen
- [ ] **Samen:** acties in sandbox-apps en misclickscenario’s testen

## Fase 7 — Speech, orb en alpha

**Resultaat:** Continuum voelt als een lokaal-eerst desktopproduct en is klaar voor een kleine alpha.

- [ ] **Toshan:** push-to-talk, STT/TTS-routing en conversational UX bouwen
- [ ] **Toshan:** orb states en animaties aan runtime-status koppelen
- [ ] **Arda:** mute-, app-exclusion- en privacycontrols afdwingen
- [ ] **Arda:** Windows installer, tray, auto-update en uninstall testen
- [ ] **Samen:** performance-, batterij-, privacy- en safety-evaluaties uitvoeren
- [ ] **Samen:** de tweedaagse “Just say continue”-demo opnemen
- [ ] **Samen:** alpha release met bekende beperkingen publiceren

## Eerstvolgende sprint

1. **Samen:** toestemming geven voor push en de blocking Windows CI-run bewijzen.
2. **Samen:** Phase 0 review; pas daarna Fase 1 starten.
3. **Arda:** eerste verticale Observer-slice: window event → privacyfilter → event store.
4. **Toshan:** hetzelfde event live tonen in de Context-pagina.

## Definition of done voor iedere taak

- Code en schema’s volgen `CONTINUUM_ARCHITECTURE.md`.
- Geen fixture of mock wordt als live functionaliteit gepresenteerd.
- Unit- en integratietests dekken success, deny en failure paths.
- Privacyfiltering gebeurt vóór opslag of externe modelcalls.
- UI-flow is met keyboard en op 1600×1000 gevalideerd.
- Buildsucces, runtimebewijs en live-integratiebewijs worden apart gerapporteerd.
- Documentatie en deze roadmap worden in dezelfde wijziging bijgewerkt.
