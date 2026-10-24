Feature: One-bar protocol — the F10 bar dance between driver and target

  Background: hub + two agents are up, machine_a holds a real ssh into machine_b
    Given the muxmesh stack is running

  # CHEMIN CRITIQUE 2 (BASE) — non délégué, prefix+c crée chez le parent seul.
  Scenario: local control creates windows at the parent only
    Then machine_a holds its prefix (not None)
    And both bars are visible before the test

  # PRESSE 1 — delegate control.
  Scenario: press 1 delegates control and shows the child bar alone
    When I press F10 on machine_a
    Then machine_a's bar is hidden
    And machine_b's bar is shown
    And machine_a's control is delegated (prefix None)
    And machine_a's client sits in the deaf key table "off"

  # CHEMIN CRITIQUE 1 (aller) — jamais deux barres, jamais zéro.
  Scenario: after press 1 exactly one bar is visible — the child's
    Then exactly one bar is visible: machine_a off, machine_b on

  # CHEMIN CRITIQUE 2 (délégué) — prefix+c crée chez l'enfant seulement.
  Scenario: during delegation new windows land at the child only
    When a window is created on machine_b
    Then machine_b has one more window and machine_a is unchanged

  # PRESSE 2 CHEZ L'ENFANT — ResumeControl via le hub.
  Scenario: press 2 at the child restores the parent's bar and control
    When I press F10 on machine_b
    Then machine_b's bar is hidden
    And machine_a's bar is restored by the hub (ResumeControl)
    And machine_a's control is reclaimed (prefix back, F10 alive in root)

  # CHEMIN CRITIQUE 1 (retour).
  Scenario: after press 2 exactly one bar is visible — the parent's
    Then exactly one bar is visible: machine_a on, machine_b off

  # CHEMIN CRITIQUE 2 (retour).
  Scenario: after reclaim new windows land at the parent only
    When a window is created on machine_a
    Then machine_a has one more window and machine_b is unchanged

  # CHEMIN CRITIQUE 3 — self-reclaim: F10 sur le parent ne meurt jamais.
  Scenario: press 2 forced on a deaf parent reawakens it (self-reclaim)
    Given machine_a is forced into the delegated state (prefix None, bar off)
    When I press F10 on machine_a
    Then machine_a's prefix is restored (self-reclaim)
    And machine_a's F10 is alive in the root key table
    And machine_a's bar is back on
