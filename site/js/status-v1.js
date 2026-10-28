"use strict";

(function () {
  const endpoint = "__VOZEN_PUBLIC_API_ORIGIN__/api/public/status";
  const healthEndpoint = "__VOZEN_PUBLIC_API_ORIGIN__/health";
  const allowed = new Set(["operational", "degraded", "unavailable"]);
  const componentStates = new Set(["operational", "degraded", "unavailable", "unknown"]);
  const labels = {
    operational: "Operational",
    degraded: "Degraded",
    unavailable: "Unavailable",
    unknown: "Not exposed",
  };
  const instance = document.getElementById("statusInstance");
  const overall = document.getElementById("statusOverall");
  const checked = document.getElementById("statusChecked");
  const latency = document.getElementById("statusLatency");
  const services = document.getElementById("statusServices");
  const incident = document.getElementById("statusIncident");
  const fields = {
    bot: { value: document.getElementById("statusBot"), card: document.getElementById("statusCheckBot") },
    database: { value: document.getElementById("statusDatabase"), card: document.getElementById("statusCheckDatabase") },
    providers: { value: document.getElementById("statusProviders"), card: document.getElementById("statusCheckProviders") },
  };

  function safeState(value) {
    return allowed.has(value) ? value : "unavailable";
  }

  function safeComponentState(value) {
    return componentStates.has(value) ? value : "unknown";
  }

  function stateLabel(state) {
    return labels[state] || labels.unavailable;
  }

  function updateClock() {
    const now = new Date();
    checked.textContent = now.toLocaleString([], { dateStyle: "medium", timeStyle: "short" });
    checked.dateTime = now.toISOString();
  }

  function render(data, measuredLatency) {
    const state = safeState(data && data.status);
    const components = (data && data.components) || {};
    const states = Object.keys(fields).map(function (key) {
      return safeComponentState(components[key]);
    });

    instance.className = "status-instance is-" + state;
    overall.className = "status-badge is-" + state;
    overall.textContent = stateLabel(state);
    services.textContent = stateLabel(state);
    latency.textContent = Number.isFinite(measuredLatency) ? Math.max(0, Math.round(measuredLatency)) + " ms" : "Unavailable";

    Object.keys(fields).forEach(function (key, index) {
      const componentState = states[index];
      fields[key].value.textContent = stateLabel(componentState);
      fields[key].card.className = "status-check is-" + componentState;
    });

    const notice = data && typeof data.incident === "string" ? data.incident.slice(0, 240) : "";
    incident.textContent = notice;
    incident.hidden = !notice;
    updateClock();
  }

  const started = performance.now();
  fetch(endpoint, { method: "GET", cache: "no-store", mode: "cors" })
    .then(function (response) {
      if (response.status === 404) {
        return fetch(healthEndpoint, { method: "GET", cache: "no-store", mode: "cors" })
          .then(function (healthResponse) {
            if (!healthResponse.ok) throw new Error("health request failed");
            return healthResponse.json();
          })
          .then(function (health) {
            const online = health && health.status === "ok";
            return {
              status: online ? "operational" : "unavailable",
              components: {
                bot: online ? "operational" : "unavailable",
                database: "unknown",
                providers: "unknown",
              },
              incident: online ? "Detailed component checks are not publicly exposed yet" : "Vozen health check is unavailable",
            };
          });
      }
      if (!response.ok) throw new Error("status request failed");
      return response.json();
    })
    .then(function (data) {
      render(data, performance.now() - started);
    })
    .catch(function () {
      render({ status: "unavailable", components: {} }, performance.now() - started);
    });
})();
