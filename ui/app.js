"use strict";

const elements = {
  authMessage: document.querySelector("#auth-message"),
  authStatus: document.querySelector("#auth-status"),
  autoRefresh: document.querySelector("#auto-refresh"),
  builderTime: document.querySelector("#builder-time"),
  builderTiming: document.querySelector("#builder-timing"),
  copyEvmWallet: document.querySelector("#copy-evm-wallet"),
  copyHelper: document.querySelector("#copy-helper"),
  copySolanaWallet: document.querySelector("#copy-solana-wallet"),
  decodeTime: document.querySelector("#decode-time"),
  decodeTiming: document.querySelector("#decode-timing"),
  evmTime: document.querySelector("#evm-time"),
  evmUnavailable: document.querySelector("#evm-unavailable"),
  evmWallet: document.querySelector("#evm-wallet"),
  openFomo: document.querySelector("#open-fomo"),
  pasteAuth: document.querySelector("#paste-auth"),
  profileInput: document.querySelector("#profile-input"),
  profileTime: document.querySelector("#profile-time"),
  profileTiming: document.querySelector("#profile-timing"),
  refreshAuth: document.querySelector("#refresh-auth"),
  resolveButton: document.querySelector("#resolve-button"),
  resolveForm: document.querySelector("#resolve-form"),
  resolveMessage: document.querySelector("#resolve-message"),
  result: document.querySelector("#result"),
  rpcInput: document.querySelector("#rpc-input"),
  rpcTime: document.querySelector("#rpc-time"),
  rpcTiming: document.querySelector("#rpc-timing"),
  solanaWallet: document.querySelector("#solana-wallet"),
  timingGrid: document.querySelector("#timing-grid"),
  totalTime: document.querySelector("#total-time"),
};

let authState = {
  autoRefresh: true,
  canRefresh: false,
  connected: false,
  expiresAt: null,
  lastRefreshAt: null,
  lastRefreshExtended: null,
  refreshError: null,
  refreshing: false,
};
let authRevision = 0;
let statusRequest = 0;

const setMessage = (element, message = "", kind = "") => {
  element.textContent = message;
  element.className = `message${kind ? ` ${kind}` : ""}`;
};

const setAuthStatus = (label, kind) => {
  elements.authStatus.textContent = label;
  elements.authStatus.className = `status status-${kind}`;
};

const requestJson = async (path, body) => {
  const response = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  const payload = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(payload.error || `Request failed (${response.status})`);
  return payload;
};

const getJson = async (path) => {
  const response = await fetch(path);
  const payload = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(payload.error || `Request failed (${response.status})`);
  return payload;
};

const copyText = async (value) => {
  await navigator.clipboard.writeText(value);
};

const formatDuration = (seconds) => {
  const safeSeconds = Math.max(0, Math.floor(seconds));
  const hours = Math.floor(safeSeconds / 3600);
  const minutes = Math.floor((safeSeconds % 3600) / 60);
  const remainder = safeSeconds % 60;
  return hours > 0
    ? `${hours}h ${minutes}m ${remainder}s`
    : `${minutes}m ${remainder}s`;
};

const liveRemainingSeconds = () =>
  authState.expiresAt === null
    ? 0
    : Math.max(0, authState.expiresAt - Math.floor(Date.now() / 1000));

const renderAuthState = () => {
  elements.autoRefresh.checked = authState.autoRefresh;
  elements.refreshAuth.disabled = !authState.canRefresh || authState.refreshing;
  elements.refreshAuth.textContent = authState.refreshing ? "Refreshing…" : "Refresh now";

  if (authState.connected && liveRemainingSeconds() > 0) {
    const remaining = formatDuration(liveRemainingSeconds());
    const refreshWasRecent =
      authState.lastRefreshAt !== null &&
      Math.floor(Date.now() / 1000) - authState.lastRefreshAt < 30;
    setAuthStatus(`Connected · ${remaining}`, "connected");
    if (authState.refreshing) {
      setMessage(elements.authMessage, "Refreshing FOMO authentication…");
    } else if (authState.refreshError) {
      setMessage(elements.authMessage, authState.refreshError, "error");
    } else if (refreshWasRecent && authState.lastRefreshExtended === true) {
      setMessage(
        elements.authMessage,
        `JWT refreshed successfully. New expiry is ${remaining}.`,
        "success",
      );
    } else if (refreshWasRecent && authState.lastRefreshExtended === false) {
      setMessage(
        elements.authMessage,
        "Refresh succeeded, but Privy kept the current JWT. Auto refresh will retry near expiry.",
      );
    } else if (authState.autoRefresh && authState.canRefresh) {
      setMessage(
        elements.authMessage,
        `FOMO connected. JWT expires in ${remaining}. Auto refresh is active.`,
        "success",
      );
    } else {
      setMessage(
        elements.authMessage,
        `FOMO connected. JWT expires in ${remaining}.`,
        "success",
      );
    }
    return;
  }

  if (authState.expiresAt !== null) {
    setAuthStatus("Expired", "error");
    setMessage(
      elements.authMessage,
      "FOMO authentication expired. Run the browser helper again.",
      "error",
    );
  } else {
    setAuthStatus("Not connected", "idle");
  }
};

const applyAuthState = (nextState) => {
  authState = { ...authState, ...nextState };
  renderAuthState();
};

const syncAuthStatus = async () => {
  const revision = authRevision;
  const request = ++statusRequest;
  try {
    const status = await getJson("/api/auth/status");
    if (revision === authRevision && request === statusRequest) {
      applyAuthState(status);
    }
  } catch {
    // The local process may be stopping. Leave the last visible state intact.
  }
};

elements.openFomo.addEventListener("click", () => {
  window.open("https://fomo.family/token", "_blank", "noopener,noreferrer");
  setMessage(
    elements.authMessage,
    "FOMO opened: log in, open DevTools → Console, paste the helper, press Enter, then wait for “fresh auth copied”",
  );
});

elements.copyHelper.addEventListener("click", async () => {
  try {
    const helper = await fetch("/browser-console.js").then((response) => response.text());
    await copyText(helper);
    elements.copyHelper.textContent = "Copied";
    setMessage(
      elements.authMessage,
      "Helper copied: open FOMO, paste it into DevTools → Console, press Enter, then wait for “fresh auth copied”",
      "success",
    );
    window.setTimeout(() => {
      elements.copyHelper.textContent = "Copy helper";
    }, 1600);
  } catch (error) {
    setMessage(elements.authMessage, error.message, "error");
  }
});

elements.pasteAuth.addEventListener("click", async () => {
  const revision = ++authRevision;
  elements.pasteAuth.disabled = true;
  elements.pasteAuth.textContent = "Connecting…";
  try {
    const authCode = await navigator.clipboard.readText();
    if (!authCode.trim().startsWith("{")) {
      throw new Error(
        "No FOMO auth is on your clipboard yet: run the helper in the FOMO console, press Enter, wait for “fresh auth copied”, then return here",
      );
    }
    const status = await requestJson("/api/auth", { authCode });
    if (revision !== authRevision) return;
    applyAuthState(status);
    try {
      if ((await navigator.clipboard.readText()) === authCode) {
        await navigator.clipboard.writeText("");
      }
    } catch {
      // Authentication is already in process memory. Clipboard cleanup is best effort.
    }
    elements.profileInput.focus();
  } catch (error) {
    if (revision === authRevision) {
      setAuthStatus("Connection failed", "error");
      setMessage(elements.authMessage, error.message, "error");
    }
  } finally {
    elements.pasteAuth.disabled = false;
    elements.pasteAuth.textContent = "Paste auth";
  }
});

elements.refreshAuth.addEventListener("click", async () => {
  const revision = ++authRevision;
  elements.refreshAuth.disabled = true;
  elements.refreshAuth.textContent = "Refreshing…";
  try {
    const status = await requestJson("/api/auth/refresh", {});
    if (revision === authRevision) applyAuthState(status);
  } catch (error) {
    if (revision === authRevision) {
      setMessage(elements.authMessage, error.message, "error");
    }
  } finally {
    renderAuthState();
  }
});

elements.autoRefresh.addEventListener("change", async () => {
  const revision = ++authRevision;
  const enabled = elements.autoRefresh.checked;
  elements.autoRefresh.disabled = true;
  try {
    const status = await requestJson("/api/auth/auto-refresh", { enabled });
    if (revision === authRevision) applyAuthState(status);
  } catch (error) {
    if (revision === authRevision) {
      elements.autoRefresh.checked = !enabled;
      setMessage(elements.authMessage, error.message, "error");
    }
  } finally {
    elements.autoRefresh.disabled = false;
  }
});

elements.resolveForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  const input = elements.profileInput.value.trim();
  if (!input) return;

  elements.resolveButton.disabled = true;
  elements.resolveButton.textContent = "Resolving…";
  elements.result.hidden = true;
  setMessage(elements.resolveMessage, "Resolving wallets…");

  try {
    const resolution = await requestJson("/api/resolve", {
      input,
      rpcUrl: elements.rpcInput.value.trim(),
    });
    elements.solanaWallet.textContent = resolution.solanaWallet;
    if (resolution.evmWallet) {
      elements.evmWallet.textContent = resolution.evmWallet;
      elements.copyEvmWallet.hidden = false;
      elements.evmUnavailable.hidden = true;
      elements.evmUnavailable.textContent = "";
    } else {
      elements.evmWallet.textContent = "Unavailable";
      elements.copyEvmWallet.hidden = true;
      elements.evmUnavailable.textContent = resolution.evmUnavailableReason;
      elements.evmUnavailable.hidden = false;
    }
    elements.totalTime.textContent = `${resolution.timings.totalMs} ms total`;
    elements.profileTime.textContent = `${resolution.timings.profileMs} ms`;
    elements.builderTime.textContent = `${resolution.timings.builderMs} ms`;
    elements.rpcTime.textContent = `${resolution.timings.rpcMs} ms`;
    elements.decodeTime.textContent = `${resolution.timings.decodeMs} ms`;
    elements.evmTime.textContent = `${resolution.timings.evmMs} ms`;
    for (const timing of [
      elements.profileTiming,
      elements.builderTiming,
      elements.rpcTiming,
      elements.decodeTiming,
    ]) {
      timing.hidden = resolution.directWalletInput;
    }
    elements.timingGrid.classList.toggle(
      "timing-grid-direct",
      resolution.directWalletInput,
    );
    elements.result.hidden = false;
    setMessage(
      elements.resolveMessage,
      resolution.evmWallet
        ? resolution.directWalletInput
          ? "EVM wallet resolved."
          : "Solana and EVM wallets resolved."
        : resolution.directWalletInput
          ? "No verified EVM wallet was found."
          : "Solana wallet resolved.",
      "success",
    );
  } catch (error) {
    setMessage(elements.resolveMessage, error.message, "error");
    if (/auth|JWT|connect/i.test(error.message)) {
      setAuthStatus("Reconnect required", "error");
      await syncAuthStatus();
    }
  } finally {
    elements.resolveButton.disabled = false;
    elements.resolveButton.textContent = "Resolve wallets";
  }
});

const bindWalletCopy = (button, wallet) => {
  button.addEventListener("click", async () => {
    try {
      await copyText(wallet.textContent);
      button.textContent = "Copied";
      window.setTimeout(() => {
        button.textContent = "Copy";
      }, 1400);
    } catch (error) {
      setMessage(elements.resolveMessage, error.message, "error");
    }
  });
};

bindWalletCopy(elements.copySolanaWallet, elements.solanaWallet);
bindWalletCopy(elements.copyEvmWallet, elements.evmWallet);

window.setInterval(renderAuthState, 1000);
window.setInterval(syncAuthStatus, 5000);
syncAuthStatus();
