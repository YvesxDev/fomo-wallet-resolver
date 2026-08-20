(async () => {
  "use strict";

  const tokenKey = (key) =>
    key === "privy:token" || /^privy:.+:token$/.test(key);
  const refreshKey = Symbol.for("fomo-wallet-resolver.refresh");

  const refreshPrivySession = async () => {
    if (window[refreshKey]) return window[refreshKey];

    const operation = (async () => {
      const moduleLink = [
        ...[...document.querySelectorAll('link[rel="modulepreload"]')].map(
          ({ href }) => href,
        ),
        ...performance.getEntriesByType("resource").map(({ name }) => name),
      ].find((href) => /\/assets\/fomoFetch-[^/]+\.js$/.test(href));
      if (!moduleLink) return null;

      const fomoModule = await import(moduleLink);
      const authenticatedFetch = Object.values(fomoModule).find(
        (candidate) =>
          typeof candidate === "function" &&
          Function.prototype.toString
            .call(candidate)
            .includes("no privy access token"),
      );
      if (!authenticatedFetch) return null;

      const originalFetch = window.fetch;
      let capturedToken = null;
      const capturingFetch = async (input, init) => {
        const headers = new Headers(
          input instanceof Request ? input.headers : undefined,
        );
        new Headers(init?.headers).forEach((value, name) =>
          headers.set(name, value),
        );
        const authorization = headers.get("authorization");
        const url = input instanceof Request ? input.url : String(input);
        if (
          url.startsWith("https://prod-api.fomo.family/") &&
          authorization?.startsWith("Bearer ")
        ) {
          capturedToken = authorization.slice("Bearer ".length).trim();
        }
        return originalFetch.call(window, input, init);
      };
      window.fetch = capturingFetch;

      try {
        await authenticatedFetch("/watchlist");
      } finally {
        if (window.fetch === capturingFetch) window.fetch = originalFetch;
      }
      return capturedToken;
    })();
    window[refreshKey] = operation;
    try {
      return await operation;
    } finally {
      if (window[refreshKey] === operation) delete window[refreshKey];
    }
  };

  const decodeClaims = (token) => {
    const parts = token.split(".");
    if (parts.length !== 3) return null;

    try {
      const encoded = parts[1].replace(/-/g, "+").replace(/_/g, "/");
      const padded = encoded.padEnd(Math.ceil(encoded.length / 4) * 4, "=");
      const bytes = Uint8Array.from(atob(padded), (character) =>
        character.charCodeAt(0),
      );
      return JSON.parse(new TextDecoder().decode(bytes));
    } catch {
      return null;
    }
  };

  const unwrapToken = (storedValue) => {
    let value = storedValue;
    for (let depth = 0; depth < 3; depth += 1) {
      if (typeof value === "string") {
        try {
          value = JSON.parse(value);
          continue;
        } catch {
          const token = value.trim();
          const claims = decodeClaims(token);
          return claims ? { token, claims } : null;
        }
      }

      if (value && typeof value === "object") {
        value = value.accessToken ?? value.token ?? value.value;
        continue;
      }
      return null;
    }
    return null;
  };

  const unwrapCredential = (storedValue) => {
    let value = storedValue;
    for (let depth = 0; depth < 3; depth += 1) {
      if (typeof value !== "string") return null;
      try {
        value = JSON.parse(value);
      } catch {
        const credential = value.trim();
        return credential || null;
      }
    }
    return typeof value === "string" && value.trim() ? value.trim() : null;
  };

  const storedCredential = (key) => {
    for (const storage of [window.localStorage, window.sessionStorage]) {
      const credential = unwrapCredential(storage.getItem(key));
      if (credential) return credential;
    }
    return null;
  };

  const isPrivyAccessToken = (candidate) =>
    candidate?.claims?.iss === "privy.io" &&
    typeof candidate.claims.aud === "string" &&
    candidate.claims.aud.length > 0 &&
    Number.isFinite(candidate.claims.exp);

  const candidates = [];
  let refreshedToken = null;
  try {
    refreshedToken = await refreshPrivySession();
  } catch (error) {
    console.warn(
      "FOMO Resolver: automatic token refresh was unavailable; checking the current token.",
      error,
    );
  }

  const refreshedCandidate = unwrapToken(refreshedToken);
  if (isPrivyAccessToken(refreshedCandidate)) {
    candidates.push(refreshedCandidate);
  }
  for (const storage of [window.localStorage, window.sessionStorage]) {
    for (let index = 0; index < storage.length; index += 1) {
      const key = storage.key(index);
      if (!key || !tokenKey(key)) continue;

      const candidate = unwrapToken(storage.getItem(key));
      if (isPrivyAccessToken(candidate)) candidates.push(candidate);
    }
  }

  candidates.sort((left, right) => right.claims.exp - left.claims.exp);
  const active = candidates.find(
    ({ claims }) => claims.exp > Math.floor(Date.now() / 1000) + 30,
  );
  if (!active) {
    const newest = candidates[0];
    if (newest) {
      const expiredBy = Math.max(
        0,
        Math.floor(Date.now() / 1000) - newest.claims.exp,
      );
      throw new Error(
        `FOMO access token found but expired ${Math.floor(expiredBy / 60)}m ${expiredBy % 60}s ago. Use FOMO once, then run this again.`,
      );
    }
    throw new Error(
      "No active FOMO access token found. Open the signed-in FOMO app, wait for it to finish loading, and run this again.",
    );
  }

  const refreshToken = storedCredential("privy:refresh_token");
  const privyAccessToken = storedCredential("privy:pat");
  const authCode = JSON.stringify({
    version: 2,
    accessToken: active.token,
    ...(refreshToken ? { refreshToken } : {}),
    ...(privyAccessToken ? { privyAccessToken } : {}),
  });
  const copyAuthCode = async () => {
    if (typeof copy === "function") {
      copy(authCode);
      return;
    }
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(authCode);
      return;
    }

    const field = document.createElement("textarea");
    field.value = authCode;
    field.style.position = "fixed";
    field.style.opacity = "0";
    document.body.appendChild(field);
    field.select();
    const copied = document.execCommand("copy");
    field.remove();
    if (!copied) throw new Error("The browser refused clipboard access.");
  };

  await copyAuthCode();
  const seconds = active.claims.exp - Math.floor(Date.now() / 1000);
  const minutes = Math.floor(seconds / 60);
  console.info(
    `FOMO Resolver: fresh auth copied. JWT expires in ${minutes}m ${seconds % 60}s.${refreshToken ? " Auto refresh ready." : ""}`,
  );
})();
