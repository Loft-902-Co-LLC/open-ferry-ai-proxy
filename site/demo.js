"use strict";

// All scenarios are readable in the HTML. Enhancement only switches visibility.
const controls = document.querySelector(".scenario-controls");
const buttons = Array.from(document.querySelectorAll("[data-scenario]"));
const scenarios = Array.from(document.querySelectorAll(".scenario"));
const announcement = document.getElementById("demo-announcement");
const outcomes = {
  healthy: "Healthy account selected. Account A is ready; the response streams back.",
  failover: "Quota failover selected. Account A returns 429 before output; account B serves the same model.",
  unavailable: "No account ready selected. Account A is cooling down, account B is disabled. With retries disabled, the sample returns 429 model_cooldown."
};

function selectScenario(id, announce) {
  if (!scenarios.some((scenario) => scenario.id === id)) return;
  for (const scenario of scenarios) scenario.hidden = scenario.id !== id;
  for (const button of buttons) {
    const selected = button.dataset.scenario === id;
    button.setAttribute("aria-pressed", String(selected));
    button.querySelector(".selection-marker").hidden = !selected;
  }
  if (announce) announcement.textContent = outcomes[id];
}

if (controls && announcement && buttons.length === 3 && scenarios.length === 3) {
  for (const button of buttons) {
    button.addEventListener("click", () => selectScenario(button.dataset.scenario, true));
  }
  selectScenario("healthy", false);
  controls.hidden = false;
}
