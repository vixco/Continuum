Continuum — volledig product- en technisch plan
1. De visie

Continuum is een AI-besturingslaag voor je computer die continu begrijpt waar je mee bezig bent, context opbouwt, onthoudt wat belangrijk is en werkzaamheden kan voortzetten zodra je zegt: “ga door.”

De gebruiker hoeft niet steeds opnieuw uit te leggen:

aan welk project hij werkt;
wat het doel is;
welke bestanden belangrijk zijn;
wat al geprobeerd is;
welke fout is ontstaan;
welke beslissingen eerder zijn genomen;
waar hij gebleven was.

Continuum draait als een Windows .exe, start automatisch met Windows en blijft stil op de achtergrond actief.

De hoofdgedachte:

Continuum ziet niet alleen je laatste bericht. Het begrijpt je huidige werksituatie.

2. De twee AI-lagen

Jullie idee om twee modellen te gebruiken is correct. Ze moeten alleen duidelijk verschillende verantwoordelijkheden krijgen.

Model 1: Context Model

Dit is het kleinere, goedkopere en liefst lokale model.

Het Context Model draait vrijwel voortdurend en is verantwoordelijk voor:

actieve applicaties herkennen;
schermveranderingen classificeren;
tekst uit vensters en screenshots begrijpen;
huidige projecten herkennen;
taken en doelen afleiden;
fouten en blokkades herkennen;
belangrijke gebeurtenissen selecteren;
sessies samenvatten;
geheugen aanmaken en organiseren;
bepalen welke context relevant is voor de Main AI.

Het Context Model voert normaal gesproken geen zware opdrachten uit.

Het denkt vooral:

Wat doet de gebruiker?
Aan welk project hoort dit?
Wat probeert hij te bereiken?
Wat veranderde er?
Is dit belangrijk?
Moet dit worden onthouden?
Is de gebruiker vastgelopen?

Dit model moet compact zijn, omdat het vaak wordt gebruikt.

Taken voor het Context Model
Event classificeren
Schermcontext samenvatten
Project herkennen
Doelconfidence berekenen
Memory candidates aanmaken
Dubbele informatie verwijderen
Sessies comprimeren
Contextpakket voorbereiden
Model 2: Main AI

Dit is het krachtigere model.

De Main AI wordt pas actief wanneer:

de gebruiker een vraag stelt;
de gebruiker “ga door” zegt;
een moeilijke taak moet worden uitgevoerd;
een probleem diep moet worden onderzocht;
een plan nodig is;
Continuum toestemming heeft gekregen om te handelen.

De Main AI doet:

complexe redenering;
programmeren;
plannen;
onderzoek;
bestanden wijzigen;
computeracties aansturen;
meerdere stappen uitvoeren;
resultaten controleren;
fouten herstellen.

Het Context Model verzamelt dus de situatie. De Main AI gebruikt die situatie om werk te doen.

Context Model:
“De gebruiker probeert een Unity-lobbyflow te repareren.
Hij heeft drie bestanden aangepast.
De fout blijft bestaan in LobbyUI.cs.
De vorige twee oplossingen werkten niet.”

Main AI:
“Ik controleer eerst de lifecycle van currentLobby, wijzig daarna
de null-handling en laat de tests opnieuw uitvoeren.”
3. De agentstructuur

Jullie hoeven niet voor iedere functie een apart groot model te draaien. De agents zijn rollen rondom de twee modellen.

Observer Agent

Kijkt naar systeemgebeurtenissen.

Verzamelt:

actieve applicatie;
actief venster;
geopende bestanden;
geselecteerde tekst;
schermveranderingen;
browserpagina;
terminaluitvoer;
IDE-errors;
Git-status;
bestandwijzigingen.

Deze agent gebruikt zo weinig mogelijk AI.

Context Agent

Gebruikt het kleine Context Model.

Hij vertaalt ruwe gebeurtenissen naar betekenis:

Ruwe gebeurtenis:
- Visual Studio Code actief
- bestand LobbyManager.cs geopend
- terminal toont NullReferenceException

Betekenis:
- gebruiker debugt de lobbyflow
- huidige blokkade is een null-reference
Goal Tracker Agent

Houdt het vermoedelijke doel bij.

Voorbeeld:

Project:
SideLife

Hoofddoel:
Handmatige lobbycreatie implementeren

Huidige taak:
ReadyZone-logica veranderen

Blokkade:
currentLobby is soms null

Confidence:
89%
Memory Curator Agent

Bepaalt wat wordt opgeslagen.

Hij voorkomt dat ieder klikje of ieder screenshot permanent in het geheugen komt.

Hij maakt onderscheid tussen:

feit;
beslissing;
voorkeur;
taak;
fout;
poging;
persoon;
bestand;
project;
doel;
resultaat.
Planner Agent

Wordt gebruikt door de Main AI.

Hij maakt een uitvoerbaar stappenplan:

1. Controleer betrokken bestanden.
2. Zoek waar currentLobby wordt aangemaakt.
3. Controleer de ReadyZone-flow.
4. Maak een herstelpunt.
5. Pas de code aan.
6. Voer tests uit.
7. Controleer regressies.
Executor Agent

Voert goedgekeurde acties uit:

bestanden openen;
code aanpassen;
browser gebruiken;
terminalcommando’s uitvoeren;
applicaties bedienen;
informatie opzoeken;
bestanden aanmaken;
formulieren invullen.
Verifier Agent

Controleert of het werk daadwerkelijk is gelukt.

Bijvoorbeeld:

compileert de code?
zijn tests geslaagd?
is het bestand correct opgeslagen?
bestaat de fout nog?
kwam de browseractie op de juiste pagina uit?
is de taak werkelijk voltooid?
Safety Agent

Controleert iedere gevoelige actie.

Deze agent mag acties blokkeren zoals:

bestanden verwijderen;
productiecode deployen;
e-mail versturen;
betalingen uitvoeren;
accounts aanpassen;
onbekende programma’s installeren;
systeeminstellingen wijzigen.
4. Hoe Continuum de computer uitleest

Continuum moet niet uitsluitend via screenshots werken. Screenshots zijn slechts één informatiebron.

Een betrouwbare desktop-AI gebruikt meerdere bronnen tegelijk.

4.1 Actieve applicatie en venster

Continuum leest:

procesnaam;
applicatienaam;
venstertitel;
vensterpositie;
actieve monitor;
tijd dat een venster actief is;
wanneer de gebruiker van venster wisselt.

Voorbeeld:

Proces: Code.exe
Venster: LobbyManager.cs — SideLife — Visual Studio Code
Actief sinds: 14 seconden

Hiermee kan Continuum vaak al herkennen aan welk project iemand werkt.

4.2 Windows Accessibility en UI Automation

Veel Windows-applicaties stellen hun interface beschikbaar via de toegankelijkheidslaag.

Continuum kan daarmee uitlezen:

knopnamen;
tekstvelden;
menu’s;
tabbladen;
geselecteerde items;
foutmeldingen;
dialogen;
lijsten;
vensterstructuur.

Dit is veel efficiënter dan alles met beeldherkenning proberen te begrijpen.

Voorbeeld:

App: Visual Studio Code
Actief tabblad: LobbyManager.cs
Open tabs:
- LobbyManager.cs
- LobbyUI.cs
- ReadyZone.cs

Problem panel:
3 errors
1 warning
4.3 Schermcapture

Voor informatie die niet via UI Automation beschikbaar is, gebruikt Continuum screenshots.

Denk aan:

canvasinterfaces;
Unity Game View;
afbeeldingen;
grafieken;
video;
custom rendered apps;
foutmeldingen die niet toegankelijk zijn;
games;
remote desktop.

De capture-service vergelijkt continu schermframes, maar analyseert niet ieder frame met AI.

4.4 Bestandssysteem

Continuum kan, na toestemming, projectmappen volgen.

Het registreert:

bestanden geopend;
bestanden gewijzigd;
bestanden aangemaakt;
bestanden verwijderd;
bestanden verplaatst;
recente projectstructuur;
wijzigingstijden;
bestandstypen.

Continuum moet niet standaard ieder persoonlijk bestand indexeren. De gebruiker selecteert mappen, bijvoorbeeld:

C:\Users\User\Projects
C:\Users\User\Documents\Continuum
D:\UnityProjects
4.5 Git-integratie

Voor programmeerprojecten leest Continuum:

actieve repository;
actieve branch;
gewijzigde bestanden;
recente commits;
diff;
commitberichten;
open conflicten;
untracked bestanden;
testresultaten.

Git geeft veel betere context dan alleen screenshots.

4.6 Terminalintegratie

Continuum kan via een terminalintegratie registreren:

uitgevoerd commando;
werkdirectory;
exitcode;
relevante output;
fouten;
duur;
actieve omgeving.

Gevoelige informatie zoals tokens en wachtwoorden moet vóór opslag worden verwijderd.

Voorbeeld:

Command:
npm run build

Exit code:
1

Detected error:
Module not found: ./LobbyService

Project:
SideLife Web
4.7 Browserextensie

Een browserextensie geeft betrouwbare webcontext:

URL;
paginatitel;
geselecteerde tekst;
zichtbare tekst;
formulierstructuur;
huidige tab;
tabwisselingen;
downloadacties;
eventueel DOM-elementen.

Privémodus, bankwebsites en wachtwoordvelden moeten standaard uitgesloten worden.

4.8 IDE-integraties

Voor de eerste doelgroep zijn plugins voor editors belangrijk.

Een VS Code- of JetBrains-plugin kan doorgeven:

actief bestand;
cursorpositie;
geselecteerde code;
diagnostics;
symbolen;
terminaloutput;
debugstatus;
actieve repository;
buildresultaten.

Hiermee begrijpt Continuum programmeerwerk veel beter.

4.9 Clipboard

Optioneel kan Continuum clipboardwijzigingen bekijken.

Dit staat standaard uit of werkt alleen binnen geselecteerde projecten.

Het systeem moet automatisch herkennen en verwijderen:

wachtwoorden;
API-keys;
creditcardnummers;
herstelcodes;
geheime tokens.
4.10 Microfoon en spraak

Spraak is een extra interface, niet de kern.

De gebruiker kan:

push-to-talk gebruiken;
microfoon permanent activeren;
alleen een wake word gebruiken;
volledig zonder spraak werken.

Speech-to-text zet de vraag om in tekst. Text-to-speech leest het antwoord voor.

De contextengine blijft hetzelfde, ongeacht of de opdracht via tekst, spraak of een sneltoets komt.

5. De frequentie van het meekijken

Iedere 0,02 seconde betekent 50 controles per seconde.

Dat is bruikbaar voor lichte schermvergelijking, maar niet voor AI-analyse.

Juiste aanpak
Schermcapture:
20–50 frames per seconde

Pixelvergelijking:
20–50 keer per seconde

Venster- en systeemevents:
direct

OCR:
alleen bij relevante verandering

Context Model:
bij betekenisvolle gebeurtenissen

Main AI:
alleen wanneer diepe redenering nodig is
Voorbeeld

Continuum ziet vijftig frames in één seconde.

Van die vijftig frames:

34 frames verschillen alleen door cursorbeweging;
10 frames zijn een scrollanimatie;
5 frames bevatten kleine wijzigingen;
1 frame toont een nieuwe foutmelding.

Alleen dat laatste frame hoeft diep verwerkt te worden.

Change detector

De change detector bepaalt:

Is alleen de cursor verplaatst?
Is er video of animatie?
Is de gebruiker aan het scrollen?
Is tekst veranderd?
Is een nieuwe popup verschenen?
Is een ander venster geopend?
Is er een foutmelding verschenen?
Is de gebruiker gestopt met typen?

Pas daarna wordt OCR of een visionmodel gebruikt.

6. De volledige contextpipeline
Computer events
↓
Capture layer
↓
Dedupe en change detection
↓
Privacy filtering
↓
Context Model
↓
Event classification
↓
Project- en doeldetectie
↓
Session state
↓
Memory Curator
↓
Short-term memory
↓
Long-term memory
↓
Context package
↓
Main AI
↓
Plan
↓
Permission check
↓
Execution
↓
Verification
↓
Memory update
Stap 1: Capture

Continuum ontvangt gebeurtenissen vanuit:

Windows;
scherm;
bestanden;
browser;
IDE;
terminal;
Git;
gebruiker.
Stap 2: Deduplicatie

Herhalende of bijna identieke events worden verwijderd.

Bijvoorbeeld honderd terminalregels kunnen één gebeurtenis worden:

Build gestart en mislukt met 14 vergelijkbare type-errors.
Stap 3: Privacyfilter

Voordat een model informatie krijgt, worden gevoelige gebieden verwijderd.

Voorbeelden:

wachtwoordvelden zwart maken;
API-keys vervangen door [REDACTED];
uitgesloten applicaties overslaan;
bankwebsites negeren;
notificaties van privéapps verbergen.
Stap 4: Classificatie

Het Context Model geeft ieder event een type.

{
  "type": "error",
  "project": "SideLife",
  "importance": 0.82,
  "confidence": 0.94,
  "summary": "Unity build failed with NullReferenceException",
  "should_store": true
}
Stap 5: Session state

Continuum houdt een live staat bij.

{
  "active_project": "SideLife",
  "current_goal": "Implement manual lobby creation",
  "current_task": "Fix ReadyZone logic",
  "active_app": "Unity",
  "open_files": [
    "LobbyManager.cs",
    "LobbyUI.cs",
    "ReadyZone.cs"
  ],
  "last_error": "NullReferenceException",
  "last_success": "Project compiled before ReadyZone change",
  "confidence": 0.88
}
Stap 6: Memory candidate

Niet ieder event wordt meteen permanente memory.

Een memory candidate kan worden bevestigd, samengevoegd of verwijderd.

Candidate:
“De gebruiker wil lobby’s altijd handmatig laten aanmaken.”

Bron:
Gesproken gebruikersopdracht

Type:
Decision

Confidence:
100%

Opslaan:
Ja
Stap 7: Contextpakket

Wanneer de Main AI nodig is, ontvangt hij geen volledige geschiedenis, maar een compact contextpakket.

Current goal
Current task
Relevant decisions
Open files
Recent changes
Failed attempts
Last successful action
Current screen
Available tools
Permissions
Recommended next step
7. Het geheugensysteem

Continuum krijgt vier geheugenlagen.

7.1 Live state

Dit is wat op dit moment gebeurt.

Voorbeelden:

actief project;
actief venster;
huidige taak;
recente fout;
laatste gebruikersopdracht.

Dit geheugen verandert voortdurend.

7.2 Event memory

Een chronologische tijdlijn van gebeurtenissen.

22:10 — Unity geopend
22:11 — ReadyZone.cs aangepast
22:13 — Build gestart
22:14 — Build mislukt
22:15 — Browser geopend
22:16 — Gebruiker zocht foutmelding op
7.3 Session memory

Een samenvatting van een werksessie.

# SideLife session — 1 augustus 2026

## Doel
ReadyZone aanpassen zodat lobby's handmatig worden aangemaakt.

## Gewijzigd
- ReadyZone.cs
- LobbyManager.cs

## Probleem
NullReferenceException bij openen van LobbyUI.

## Geprobeerd
- Automatische lobbycreatie verwijderd.
- Null-check toegevoegd in ReadyZone.

## Resultaat
Build slaagt, maar lobby-UI opent nog niet.

## Volgende stap
Controleer wanneer currentLobby aan LobbyUI wordt doorgegeven.
7.4 Long-term memory

Dit zijn blijvende feiten, beslissingen en voorkeuren.

Voorbeelden:

Beslissing:
Lobby's worden nooit automatisch aangemaakt.

Voorkeur:
Gebruiker wil eerst een werkende MVP, daarna visuele polish.

Projectregel:
Alle API-routes gebruiken dezelfde error response.

Persoon:
Harrie werkt aan de backend.
8. Obsidian-achtige opslag

De gebruiker moet zijn geheugen kunnen openen en bewerken.

Een mogelijke structuur:

ContinuumVault/
├── Projects/
│   ├── SideLife.md
│   └── Continuum.md
├── Goals/
├── Decisions/
├── People/
├── Preferences/
├── Tasks/
├── Sessions/
├── Errors/
├── Knowledge/
└── Daily/

Markdownvoorbeeld:

---
type: decision
project: SideLife
created: 2026-08-01T21:45:00
confidence: 1.0
importance: 0.92
source: user_statement
sensitivity: internal
---

# Lobby creation must be manual

The ReadyZone may open the lobby interface, but it may not
automatically create a lobby.

Related:
- [[SideLife]]
- [[ReadyZone]]
- [[Lobby flow redesign]]

Markdown is bedoeld voor transparantie en export.

Daarnaast gebruikt Continuum intern:

SQLite voor snelle queries;
embeddings voor semantisch zoeken;
een graphlaag voor relaties;
een tijdelijke eventbuffer;
versleutelde objectopslag voor snapshots.
9. Het knowledge graph

De Memory-pagina uit jullie ontwerp kan relaties laten zien tussen:

Project
Goal
Task
Decision
Person
File
Error
Fact
Preference
Session
Agent run

Voorbeeld:

SideLife
├── heeft doel → Lobbyflow veranderen
├── bevat bestand → ReadyZone.cs
├── bevat beslissing → Lobby's handmatig aanmaken
├── heeft blokkade → NullReferenceException
├── uitgevoerd door → Builder Agent
└── besproken in → Session 1 augustus

Iedere relatie krijgt metadata:

{
  "from": "decision_manual_lobby",
  "to": "project_sidelife",
  "relation": "belongs_to",
  "confidence": 1.0,
  "source": "user_statement"
}
10. Hoe “ga door” exact werkt

“Ga door” moet een speciaal commando zijn, niet alleen een normale prompt.

Stap 1: identificeer de context

Continuum controleert:

actief project;
huidig venster;
recente sessie;
laatste onafgemaakte taak;
laatste fout;
laatste gebruikersopdracht;
tijd sinds laatste activiteit.
Stap 2: bepaal waar “dit” naar verwijst

Continuum maakt kandidaten:

Kandidaat 1:
SideLife lobbyflow
Confidence: 91%

Kandidaat 2:
Continuum UI-design
Confidence: 22%

Kandidaat 3:
Browseronderzoek naar Unity errors
Confidence: 17%

Bij hoge confidence gaat Continuum verder.

Bij lage confidence vraagt het:

“Bedoel je de SideLife-lobbyflow of het Continuum-dashboard?”

Stap 3: bouw het contextpakket
Project:
SideLife

Goal:
Manual lobby creation

Current task:
Fix ReadyZone and LobbyUI handoff

Relevant files:
- ReadyZone.cs
- LobbyManager.cs
- LobbyUI.cs

Previous attempts:
- Removed automatic creation
- Added null check

Current blocker:
LobbyUI opens without currentLobby

Last successful state:
Project compiled before the UI change

Permission level:
May edit project files and run local tests
May not push or deploy
Stap 4: Main AI maakt plan
1. Open de drie betrokken bestanden.
2. Traceer waar currentLobby wordt gezet.
3. Controleer de eventvolgorde.
4. Maak een Git-herstelpunt.
5. Pas de overdracht aan.
6. Compileer.
7. Voer lobbyscenario uit.
Stap 5: permissiecontrole

Veilige lokale analyse kan automatisch.

Code aanpassen kan bijvoorbeeld binnen een goedgekeurde projectmap.

Git push, deploy of externe communicatie vereist extra toestemming.

Stap 6: uitvoering

De Executor gebruikt:

bestands-API’s;
IDE-integratie;
terminal;
browser;
Windows UI Automation.
Stap 7: verificatie

Continuum controleert:

is de code opgeslagen?
compileert het project?
is de fout verdwenen?
werkt het bedoelde scenario?
zijn nieuwe fouten ontstaan?
Stap 8: memory update

Na de uitvoering wordt opgeslagen:

Actie:
LobbyUI handoff aangepast.

Resultaat:
Build geslaagd.

Nieuw probleem:
Ready-knop blijft uitgeschakeld.

Volgende stap:
Controleer ReadyState synchronisatie.
11. Computerbesturing

Continuum moet acties uitvoeren via een duidelijke voorkeursvolgorde.

Niveau 1: directe integratie

Dit is het betrouwbaarst.

Voorbeelden:

bestand direct wijzigen;
Git-commando uitvoeren;
browser-API gebruiken;
IDE-plugin gebruiken;
lokale applicatie-API aanroepen.
Niveau 2: Windows UI Automation

Continuum zoekt knoppen en velden via hun semantische naam.

Find button: “Build”
Invoke button
Wait for result
Read status panel

Dit is beter dan klikken op vaste coördinaten.

Niveau 3: browser-DOM

De browserextensie kan:

elementen selecteren;
tekstvelden invullen;
knoppen activeren;
tabellen lezen;
downloads starten.
Niveau 4: visuele bediening

Als geen API of accessibility-data beschikbaar is, gebruikt Continuum:

screenshot;
objectdetectie;
muiscoördinaten;
toetsenbordinput.

Dit is de minst betrouwbare methode en moet als fallback worden gebruikt.

12. Permissiesysteem

De gebruiker moet per agent, app, project en actie rechten kunnen geven.

Observatierechten
Actieve app zien
Venstertitel zien
Scherm bekijken
Bestanden lezen
Browserpagina lezen
Clipboard lezen
Microfoon gebruiken
Terminaluitvoer lezen
Actierechten
Bestanden schrijven
Terminalcommando’s uitvoeren
Browser bedienen
Muis en toetsenbord bedienen
Programma’s openen
E-mails voorbereiden
E-mails versturen
Git commits maken
Git push uitvoeren
Software installeren
Autonomieniveaus
Niveau 0 — Uit

Continuum observeert niets.

Niveau 1 — Observe

Continuum kijkt en onthoudt, maar voert niets uit.

Niveau 2 — Suggest

Continuum stelt acties voor.

Niveau 3 — Supervised

Continuum voert veilige stappen uit en vraagt toestemming voor wijzigingen.

Niveau 4 — Project autonomy

Continuum mag zelfstandig werken binnen één geselecteerd project.

Niveau 5 — Extended autonomy

Continuum mag meerdere systemen bedienen binnen duidelijke beleidsregels.

De eerste release hoeft maximaal tot niveau 3 te gaan.

13. Privacyontwerp

Omdat Continuum veel kan zien, moet privacy zichtbaar en controleerbaar zijn.

Standaardregels
lokale verwerking als uitgangspunt;
geen schermbeelden naar cloud zonder toestemming;
wachtwoordvelden automatisch verbergen;
uitgesloten apps nooit capturen;
ruwe screenshots snel verwijderen;
alleen samenvattingen langdurig bewaren;
iedere memory heeft een bron;
iedere agentactie komt in een auditlog;
gebruiker kan een volledige periode verwijderen.
Privacyzones

Gebruiker kan regels instellen:

Nooit observeren:
- Bitwarden
- Banking apps
- WhatsApp
- Incognito browsers

Alleen lokaal:
- Visual Studio Code
- Unity
- Documents

Cloud toegestaan:
- Publieke websites
- Open-source projecten
Privacyknoppen

De interface moet apart tonen:

Microfoon aan/uit
Schermobservatie aan/uit
Bestandsmonitoring aan/uit
Agentuitvoering aan/uit
Alles pauzeren

Mute mag niet onduidelijk betekenen dat alleen audio uitstaat terwijl schermobservatie doorgaat.

14. Modelverbindingen

Bij de eerste start ziet de gebruiker:

Optie A: lokaal model

De gebruiker kiest een lokaal endpoint.

Continuum test:

is de server bereikbaar?
welke modellen zijn beschikbaar?
ondersteunt het model vision?
ondersteunt het tool calls?
hoeveel context kan het verwerken?
is het snel genoeg?
Optie B: API-provider

De gebruiker voert een API-key in.

De key wordt opgeslagen in de Windows Credential Manager of een versleutelde secret store, niet in een normaal configuratiebestand.

Optie C: hybride modus

Aanbevolen opzet:

Context Model:
Lokaal

Embeddings:
Lokaal

OCR:
Lokaal

Main AI:
Cloud of krachtig lokaal model

Gevoelige taken:
Alleen lokaal

Complexe niet-gevoelige taken:
Cloud toegestaan
15. Modelrouter

De modelrouter kiest welk model een opdracht krijgt.

Taak: classificeren
→ Context Model

Taak: sessie samenvatten
→ Context Model

Taak: eenvoudige vraag beantwoorden
→ Context Model of goedkope Main AI

Taak: moeilijke codebug oplossen
→ Main AI

Taak: complex onderzoek uitvoeren
→ Main AI

Taak: gevoelige bestanden analyseren
→ Lokaal model

Taak: snelle visioncontrole
→ Klein visionmodel

De router kijkt naar:

moeilijkheid;
gevoeligheid;
gewenste snelheid;
kosten;
benodigde tools;
benodigde contextlengte;
modelbeschikbaarheid.
16. Softwarearchitectuur

Continuum bestaat niet uit één groot proces.

Desktop UI

Verantwoordelijk voor:

dashboard;
memory graph;
agents;
permissions;
timeline;
settings;
orb;
notificaties.

Geschikte richting:

Tauri
React
TypeScript
Core Service

Een achtergrondservice die ook blijft draaien wanneer het dashboard gesloten is.

Verantwoordelijk voor:

sessiestatus;
eventroutering;
modelrouter;
memory;
agents;
permissions;
task queue.

Geschikte richting:

Rust of C#
Capture Service

Verantwoordelijk voor:

schermcapture;
actieve vensters;
UI Automation;
procesinformatie;
input-idle status;
monitoren;
change detection.
Integration Host

Beheert:

browserextensie;
VS Code-plugin;
terminalplugin;
Git;
bestandssysteem;
externe tools.
Model Gateway

Een uniforme interface voor lokale en cloudmodellen.

generate()
stream()
embed()
vision()
transcribe()
speak()
tool_call()
Agent Runtime

Beheert:

taken;
plannen;
agentstatus;
retries;
timeouts;
tools;
uitvoer;
verificatie.
Permission Engine

Controleert iedere tool call.

Agent vraagt:
write_file("LobbyUI.cs")

Permission Engine:
Project toegestaan: ja
Bestand binnen projectmap: ja
Actieklasse: lokale wijziging
Autonomieniveau: supervised
Herstelpunt aanwezig: ja

Besluit:
toestaan
Tool Sandbox

Gevoelige acties worden geïsoleerd uitgevoerd.

Voorbeelden:

commando’s met timeouts;
toegestane directories;
geblokkeerde systeemcommando’s;
netwerkbeperkingen;
resource limits.
17. Processtructuur
Continuum.exe
├── Continuum UI
├── Continuum Core Service
├── Capture Worker
├── Context Worker
├── Memory Worker
├── Agent Worker
└── Update Service

Processen communiceren lokaal via:

named pipes;
lokale sockets;
authenticated localhost-interface.

De UI hoeft niet altijd open te staan. De Core Service blijft functioneren.

18. Databasestructuur
Events
id
timestamp
source
application
window
project_id
event_type
raw_reference
summary
importance
sensitivity
confidence
Memories
id
type
title
content
project_id
source_event_ids
confidence
importance
sensitivity
created_at
updated_at
last_used_at
expires_at
confirmed
Projects
id
name
root_paths
repositories
applications
goals
permissions_profile
created_at
last_active_at
Tasks
id
project_id
goal_id
status
description
plan
current_step
assigned_agent
created_at
completed_at
Agent runs
id
agent
task_id
model
started_at
ended_at
status
input_tokens
output_tokens
tools_used
result
error
Permissions
subject
resource
action
scope
decision
expires_at
requires_confirmation
19. De interface

Jullie bestaande ontwerp kan de hoofdapp worden.

Home

Moet direct tonen:

Current focus
Current goal
Current task
Detected blocker
Observation status
Main AI status
Recent agent activity
Continue button
Projects

Per project:

doel;
voortgang;
actieve taak;
bestanden;
repositories;
recente sessies;
beslissingen;
agents;
toegestane acties.
Context

Deze pagina ontbreekt nog en is belangrijk.

Hier ziet de gebruiker wat Continuum nu denkt:

Active project:
Continuum Desktop

Current goal:
Implement local model setup

Current activity:
Debugging provider connection

Detected blocker:
Model endpoint timeout

Confidence:
86%

Sources:
- Active window
- Terminal output
- Previous session
- Git diff

De gebruiker kan correcties geven:

[Correct]
[Forget]
[Pin as goal]
[Not related to this project]
Memory

Jullie huidige graphdesign past hier goed.

Functionaliteit:

memories zoeken;
filteren op type;
bron bekijken;
confidence aanpassen;
relaties zien;
verkeerde memories verwijderen;
memories bevestigen;
opgeslagen views.
Agents

Toont:

actieve agents;
lopende taak;
gebruikte tools;
stappen;
output;
kosten;
pauzeren;
annuleren;
permissions;
gebruikte model.
Permissions

Toont:

apprechten;
projectrechten;
agentrechten;
tijdelijke goedkeuringen;
geblokkeerde acties;
gevoelige apps;
autonomieprofielen.
Timeline

Een volledig controleerbaar logboek:

22:10 — VS Code werd actief
22:11 — Project SideLife herkend
22:12 — ReadyZone.cs gewijzigd
22:14 — Build mislukt
22:14 — Memory aangemaakt
22:15 — Builder Agent gestart
22:16 — Toestemming gevraagd
22:17 — Bestand aangepast
22:18 — Build geslaagd
Settings

Onderdelen:

modellen;
API-keys;
lokale endpoints;
opslag;
privacy;
observerfrequentie;
speech;
startup;
browserextensie;
IDE-plugins;
updatekanaal;
export en verwijdering.
20. De orb en snelle interactie

De orb is een extra laag boven de desktop.

Statussen:

Grijs:
gepauzeerd

Blauw:
observeren

Paars:
luisteren

Wit:
denken

Groen:
uitvoeren

Oranje:
toestemming nodig

Rood:
geblokkeerd of fout

Bij klikken:

Current project
Current goal
Last observation
Ask Continuum
Continue
Pause observation
Open dashboard

Sneltoets:

Ctrl + Space:
Continuum openen

Ctrl + Shift + Space:
Push-to-talk

Ctrl + Alt + C:
Ga door

Ctrl + Alt + P:
Alles pauzeren
21. Prestatiebudget

Continuum mag de computer niet merkbaar vertragen.

Idle

Wanneer gebruiker niets doet:

CPU:
bijna nul

GPU:
nul

Schermanalyse:
gepauzeerd

Geheugen:
core service actief
Normaal werk
Framevergelijking:
lichtgewicht

Context Model:
alleen bij events

OCR:
selectief

Main AI:
inactief tot nodig
Zwaar werk

Wanneer een agent actief een taak uitvoert:

CPU/GPU-gebruik mag tijdelijk stijgen;
gebruiker ziet dit in het dashboard;
taak kan worden gepauzeerd;
er zijn resource limits.

Instellingen:

Eco
Balanced
Performance
22. Memory-compressie

Zonder compressie groeit het geheugen te snel.

Continuum gebruikt meerdere niveaus.

Ruwe events

Bewaren gedurende bijvoorbeeld korte tijd.

Screenshot
Vensterwissel
Terminalregel
Klik-event
Samengevoegde events

Na enkele minuten:

Gebruiker wijzigde drie lobbybestanden en testte de build tweemaal.
Sessiesamenvatting

Na projectwisseling of inactiviteit:

De ReadyZone-flow werd aangepast. Build werkt, UI-koppeling blijft defect.
Permanente memory

Alleen blijvende informatie:

Beslissing: lobby's worden handmatig aangemaakt.

Ruwe screenshots hoeven doorgaans niet permanent bewaard te worden.

23. Contextvervuiling voorkomen

Iedere memory krijgt:

Confidence
Importance
Source
Sensitivity
Project
Timestamp
Expiry
Confirmation
Last used
Contradictions

Continuum moet conflicten herkennen.

Voorbeeld:

Oude beslissing:
Gebruik MongoDB.

Nieuwe beslissing:
Gebruik PostgreSQL.

Resultaat:
Oude beslissing markeren als superseded.
Niet beide presenteren als actuele waarheid.

Het systeem moet onderscheid maken tussen:

De gebruiker zei dit expliciet.
Een agent concludeerde dit.
Het systeem vermoedt dit.
Dit was vroeger waar.
Dit is bevestigd.
Dit is achterhaald.
24. Veilig uitvoeren en terugdraaien

Voor iedere wijzigende actie maakt Continuum waar mogelijk een herstelpunt.

Code
nieuwe Git-branch;
diff opslaan;
gewijzigde bestanden tonen;
rollback ondersteunen.
Documenten
vorige versie bewaren;
autosavekopie;
wijzigingsoverzicht.
Browser
concept voorbereiden vóór verzenden;
geen aankoop zonder toestemming;
geen formulier definitief verzenden zonder toestemming.
Terminal
gevaarlijke commando’s blokkeren;
directory beperken;
timeout instellen;
output loggen.
Systeem
geen administratorrechten als standaard;
installatie en systeemwijzigingen apart bevestigen.
25. Foutafhandeling

Een agent mag niet oneindig blijven proberen.

Voorbeeldbeleid:

Poging 1:
normale oplossing

Poging 2:
alternatieve aanpak

Poging 3:
meer context verzamelen

Daarna:
stoppen en gebruiker informeren

Continuum meldt:

Ik heb drie aanpakken geprobeerd.

Wat werkte niet:
- endpoint opnieuw verbinden
- timeout verhogen
- modelserver herstarten

Waarschijnlijke oorzaak:
poort wordt door een ander proces gebruikt

Aanbevolen volgende stap:
controleer proces 14320

Toestemming nodig:
proces beëindigen
26. MVP: wat eerst gebouwd moet worden

De eerste versie moet niet alles kunnen.

MVP-doel

Bewijzen dat Continuum betrouwbaar kan beantwoorden:

“Waar was ik mee bezig?”

en:

“Ga door.”

MVP-functies
Windows-app
installer;
.exe;
auto-start;
background service;
tray icon;
dashboard;
compacte orb.
Observatie
actieve app;
venstertitel;
betekenisvolle screenshots;
geselecteerde projectmappen;
bestandwijzigingen;
Git-context;
eenvoudige browserextensie;
eenvoudige VS Code-plugin.
Modellen
één lokaal Context Model;
één instelbare Main AI;
modelrouter;
lokale/API-configuratie.
Memory
events;
sessies;
projecten;
taken;
beslissingen;
Markdown-export;
SQLite;
eenvoudig semantisch zoeken.
Interactie
tekstinput;
“ga door”;
“wat deed ik?”;
“waar liep ik vast?”;
“wat veranderde er vandaag?”;
optionele STT/TTS.
Uitvoering

In versie één alleen:

projectbestanden lezen;
voorgestelde wijzigingen voorbereiden;
terminalcommando’s uitvoeren na toestemming;
browser openen;
relevante bestanden openen;
code-agent starten.

Nog geen volledige autonome desktopbesturing.

27. Ontwikkelfases
Fase 1 — Observer en live context

Bouw:

Windows service;
app/window tracking;
filesystem watcher;
screenshot change detector;
eventdatabase;
projectdetectie;
live Context-pagina.

Resultaat:

Continuum weet wat actief is en kan de laatste werkzaamheden samenvatten.

Fase 2 — Geheugen

Bouw:

session summarization;
memory types;
confidence;
knowledge graph;
Markdown vault;
zoekfunctie;
memory review;
privacyregels.

Resultaat:

Continuum onthoudt projecten en beslissingen over meerdere dagen.

Fase 3 — Twee-modellenarchitectuur

Bouw:

Context Model runtime;
Main AI gateway;
context packager;
modelrouter;
kosten- en privacybeleid;
lokale/API-selectie.

Resultaat:

Het kleine model verzamelt context en de grote AI lost moeilijke taken op.

Fase 4 — “Ga door”

Bouw:

task-state machine;
goal tracker;
unfinished-task detection;
continuation resolver;
planner;
permission checks;
verifier.

Resultaat:

De gebruiker kan na een pauze zeggen: “ga door.”

Fase 5 — Acties uitvoeren

Bouw:

tool system;
terminal tool;
file tool;
browser tool;
IDE tool;
rollback;
audit log.

Resultaat:

Continuum kan gecontroleerd werk uitvoeren.

Fase 6 — Desktop control

Bouw:

UI Automation;
browser-DOM control;
visuele fallback;
mouse/keyboard control;
safe action confirmation.

Resultaat:

Continuum kan ook apps zonder directe integratie bedienen.

Fase 7 — Speech en orb

Bouw:

push-to-talk;
lokale/cloud-STT;
TTS;
wake word optioneel;
orb animations;
mute- en privacycontrols.

Resultaat:

Continuum voelt als een levende desktopassistent.

28. Eerste praktische teamverdeling
Developer 1 — Windows en systemen
background service;
capture;
UI Automation;
filesystem;
process tracking;
installer;
performance.
Developer 2 — AI en backend
modelgateway;
context pipeline;
memory;
agents;
prompts;
routing;
evaluations.
Developer 3 — Frontend en product
dashboard;
orb;
memory graph;
agent monitoring;
permissions;
onboarding.

Met twee mensen kan het ook, maar dan moet de eerste versie veel smaller blijven.

29. Belangrijkste technische risico’s
Verkeerd doel afleiden

Oplossing:

confidence tonen;
bronnen tonen;
gebruiker laten corrigeren;
geen permanente memory bij lage confidence.
Te veel context opslaan

Oplossing:

events comprimeren;
alleen betekenisvolle informatie bewaren;
expiry gebruiken;
memory curator bouwen.
Computer wordt traag

Oplossing:

change detection zonder AI;
event-driven verwerking;
resource limits;
modellen alleen activeren wanneer nodig.
Privacyverlies

Oplossing:

lokaal standaard;
redaction vóór modelgebruik;
app exclusions;
duidelijke privacyknoppen;
auditlog;
versleuteling.
Agents maken fouten

Oplossing:

permissions;
rollback;
verifier;
maximaal aantal pogingen;
sandbox;
previews.
Main AI krijgt verkeerde context

Oplossing:

bronnen meegeven;
confidence;
conflicten herkennen;
contextpakket testen;
belangrijke memories bevestigen.
30. Evaluaties

Jullie moeten niet alleen testen of een model goede tekst maakt.

Context recall-test

Na twintig minuten werken:

“Waar is de gebruiker mee bezig?”

Continuum moet correct herkennen:

project;
doel;
huidige taak;
blokkade;
laatste actie.
Continuation-test

Na het sluiten en opnieuw openen:

“Ga door.”

Continuum moet de juiste taak kiezen en een logisch vervolg voorstellen.

Memory precision-test

Van honderd opgeslagen memories:

hoeveel waren werkelijk belangrijk?
hoeveel waren dubbel?
hoeveel waren verkeerd?
hoeveel werden later nuttig gebruikt?
Safety-test

Kan Continuum:

een verwijderactie blokkeren?
geheime tokens redacteren?
een uitgesloten app negeren?
een actie terugdraaien?
stoppen na intrekken van toestemming?
Performance-test

Met Continuum actief:

blijft inputrespons goed?
blijft CPU-gebruik acceptabel?
groeit opslag beheersbaar?
blijft batterijgebruik redelijk?
31. De eerste virale demo
Dag één

De gebruiker opent een project en werkt eraan.

Continuum observeert:

bestanden;
vensters;
terminal;
fouten;
browseronderzoek;
beslissingen.

De gebruiker zegt niets tegen Continuum en sluit de computer.

Dag twee

De gebruiker start de computer en zegt:

“Ga door.”

Continuum antwoordt:

“Je was bezig met de handmatige lobbyflow in SideLife. De automatische lobbycreatie is verwijderd, maar LobbyUI ontvangt currentLobby nog te vroeg. Je laatste build slaagde, alleen de Ready-knop bleef uitgeschakeld. Ik kan de betrokken bestanden openen, een herstelpunt maken en de eventvolgorde corrigeren.”

Daarna voert Continuum, met toestemming, de volgende stap uit.

Eindtekst:

Continuum — your AI already knows.

Of:

Just say continue.

32. De kernarchitectuur samengevat
CONTINUUM

INPUT LAYER
├── Screen
├── Windows
├── Files
├── Git
├── Browser
├── IDE
├── Terminal
├── Clipboard
└── Voice

CONTEXT LAYER
├── Observer Agent
├── Change Detector
├── Privacy Filter
├── Context Model
├── Goal Tracker
└── Memory Curator

MEMORY LAYER
├── Live State
├── Event Timeline
├── Session Memory
├── Long-term Memory
├── Knowledge Graph
└── Markdown Vault

REASONING LAYER
├── Main AI
├── Planner Agent
├── Research Agent
├── Executor Agent
├── Verifier Agent
└── Safety Agent

ACTION LAYER
├── Files
├── Terminal
├── Browser
├── IDE
├── Windows UI
├── Mouse
└── Keyboard

CONTROL LAYER
├── Permissions
├── Audit Log
├── Rollback
├── Privacy Zones
├── Resource Limits
└── Emergency Stop
Definitieve productdefinitie

Continuum is een lokaal-eerst AI-systeem voor Windows dat voortdurend systeem-, applicatie- en projectcontext verzamelt. Een licht Context Model observeert, structureert en onthoudt wat de gebruiker doet. Een krachtigere Main AI gebruikt die opgebouwde context om vragen te beantwoorden, plannen te maken en goedgekeurde taken op de computer voort te zetten.

Het onderscheidende element is niet alleen dat Continuum de computer kan bedienen.

Het onderscheidende element is:

Wanneer Continuum een taak uitvoert, weet het al waarom de gebruiker die taak uitvoert, wat eerder is geprobeerd en wat het gewenste eindresultaat is.