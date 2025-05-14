# Thema: Completely Fair Scheduler  
## Bearbeitet von David Schwabauer und Lazar Konstantinou

### Was gibt es schon in D3OS? 
* Einen simplen Scheduler mit grundlegenden Funktionen wie switch_thread()

### Mögliche Hilfmittel:
* Bereits existierende CFS auf GitHub
* Bachelorarbeit eines Kommilitonen über den Completely Fair Scheduler auf hhuOS (keine Antwort bisher)

### Recherche:
* Literatur heraussuchen wie ein Scheduler in Rust implementiert wird  
* Literatur heraussuchen, wie der Completely Fair Scheduler genau funktioniert

### Implementierung Completely Fair Scheduler: 
* Rot-Schwarz Baum Datenstruktur raussuchen und für CFS umimplementieren, da Eigenentwicklung des gesamten Rot-Schwarz Baums den Rahmen sprengt
* Threads innerhalb des Rot-Schwarz Baumes speichern
* Virtual runtime für Threads bestimmen und zur Laufzeit (und je nach Priorität) anpassen können
* Thread wechseln, sobald ein Thread mit niedrigerer virtual runtime vorliegt

### Vortrag:
* D3OS kurz vorstellen
* CFS vorstellen
	* Was macht ihn so besonders?
	* Wieso verwendet Linux ihn? 
	* Wie genau funktioniert er?
	* ...
* Vergleichbare Scheduler kurz anmerken & somit auch heraussuchen und recherchieren  

### Ausarbeitung:
* Besonderheiten CFS und Implementierung beschreiben
