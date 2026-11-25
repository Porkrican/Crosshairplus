"use strict";

const S = {
  modules: { crosshair: false, display: false, audio: false },
  slots: [],
  selectedSlot: null,
};

function send(type, payload = {}) {
  try {
    window.ipc.postMessage(JSON.stringify({ v: 1, type, payload }));
  } catch (error) {
    console.error("Tray IPC unavailable", error);
  }
}

function render() {
  document.querySelectorAll(".tray-toggle").forEach((button) => {
    const on = !!S.modules[button.dataset.module];
    button.setAttribute("aria-pressed", String(on));
  });
  const list = document.querySelector("#traySlots");
  const count = document.querySelector("#traySlotCount");
  if (!list || !count) return;
  count.textContent = String(S.slots.length);
  list.replaceChildren();
  if (!S.slots.length) {
    const empty = document.createElement("div");
    empty.className = "tray-empty";
    empty.textContent = "No slots saved";
    list.appendChild(empty);
    return;
  }
  S.slots.forEach((slot) => {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "tray-slot" + (slot.id === S.selectedSlot ? " active" : "");
    button.dataset.slot = slot.id;
    button.setAttribute("aria-pressed", String(slot.id === S.selectedSlot));

    const emoji = document.createElement("span");
    emoji.className = "tray-slot-emoji";
    emoji.textContent = slot.emoji || "⚡";
    const name = document.createElement("span");
    name.className = "tray-slot-name";
    name.textContent = slot.name;
    const state = document.createElement("span");
    state.className = "tray-slot-state";
    button.append(emoji, name, state);
    list.appendChild(button);
  });
}

window.__crosshairTrayState = (next) => {
  S.modules = next?.modules || S.modules;
  S.slots = Array.isArray(next?.slots) ? next.slots : [];
  S.selectedSlot = next?.selectedSlot || null;
  render();
};

document.addEventListener("click", (event) => {
  const toggle = event.target.closest?.(".tray-toggle");
  if (toggle) {
    const module = toggle.dataset.module;
    S.modules[module] = !S.modules[module];
    render();
    send("ToggleMaster", { module });
    return;
  }
  const slot = event.target.closest?.(".tray-slot");
  if (slot) {
    S.selectedSlot = slot.dataset.slot;
    render();
    send("ActivateSlot", { id: slot.dataset.slot });
    return;
  }
  const action = event.target.closest?.("[data-action]");
  if (action) send(action.dataset.action);
});

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") send("HideTray");
});

render();
