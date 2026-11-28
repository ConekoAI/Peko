import { describe, it, expect } from "vitest";
import { parseDeepLink } from "../lib/deepLink";

/**
 * PR #6: deep-link URL parser. Pure function — no side effects —
 * so we can pin the wire-shape contract without instantiating the
 * Tauri plugin runtime. The contract under test is the one the OS
 * hands the desktop when the user clicks a share link from another
 * app; any drift here means a real share link silently fails.
 *
 * The emitted forms are `peko://add-peko` and the peko's page
 * `https://<hub>/{owner}/{name}` (pekohub ADR-009, amended 2026-11-28);
 * the older `peko://add-principal` intent still parses on receipt, and
 * the retired pages (`/@{owner}/{name}`, `/peko/…`, `/p/…`) do not.
 */

describe("parseDeepLink", () => {
  describe("the peko's page", () => {
    it("accepts add-peko wrapping an /owner/name link", () => {
      const inner = "https://pekohub.ai/alice/coding-assistant";
      expect(parseDeepLink(`peko://add-peko?url=${encodeURIComponent(inner)}`)).toEqual({
        kind: "add-principal",
        shareUrl: inner,
      });
    });

    it("accepts add-peko with a token-bearing link", () => {
      const inner = "https://pekohub.ai/alice/coding-assistant?token=abc123";
      expect(parseDeepLink(`peko://add-peko?url=${encodeURIComponent(inner)}`)).toEqual({
        kind: "add-principal",
        shareUrl: inner,
      });
    });

    it("accepts a bare /owner/name link handed over by the OS", () => {
      const raw = "https://pekohub.ai/alice/coding-assistant?token=abc";
      expect(parseDeepLink(raw)).toEqual({ kind: "pekohub-share", shareUrl: raw });
    });

    it("accepts a self-hosted hub and plain http", () => {
      for (const url of ["https://hub.example.com/alice/coding-assistant", "http://localhost:8080/alice/coding-assistant"]) {
        expect(parseDeepLink(url)).toEqual({ kind: "pekohub-share", shareUrl: url });
      }
    });

    it("rejects a person's page and a seed's page", () => {
      expect(parseDeepLink("https://pekohub.ai/alice")).toBeNull();
      expect(parseDeepLink("https://pekohub.ai/alice/seeds/ada")).toBeNull();
    });

    it("rejects the retired pages", () => {
      for (const url of [
        "https://pekohub.ai/@alice/coding-assistant",
        "https://pekohub.org/peko/alice/coding-assistant",
        "https://pekohub.org/p/alice/coding-assistant",
      ]) {
        expect(parseDeepLink(url)).toBeNull();
        expect(parseDeepLink(`peko://add-peko?url=${encodeURIComponent(url)}`)).toEqual({
          kind: "add-principal",
          shareUrl: url,
        });
      }
    });
  });

  describe("the API URL forms", () => {
    it("accepts /v1/public/pekos/{owner}/{name}", () => {
      const url = "https://pekohub.org/v1/public/pekos/alice/coding-assistant";
      expect(parseDeepLink(url)).toEqual({ kind: "pekohub-share", shareUrl: url });
    });

    it("accepts the legacy /v1/public/principals/{owner}/{name}", () => {
      const url = "https://pekohub.org/v1/public/principals/alice/coding-assistant";
      expect(parseDeepLink(url)).toEqual({ kind: "pekohub-share", shareUrl: url });
    });
  });

  describe("peko:// scheme", () => {
    it("accepts the legacy add-principal intent", () => {
      const inner = "https://pekohub.ai/alice/coding-assistant";
      expect(parseDeepLink(`peko://add-principal?url=${encodeURIComponent(inner)}`)).toEqual({
        kind: "add-principal",
        shareUrl: inner,
      });
    });

    it("rejects unknown peko:// hostnames", () => {
      expect(parseDeepLink("peko://open-principal/alice/coding-assistant")).toBeNull();
      expect(parseDeepLink("peko://settings")).toBeNull();
    });

    it("rejects add-peko and add-principal without the url= query parameter", () => {
      expect(parseDeepLink("peko://add-peko")).toBeNull();
      expect(parseDeepLink("peko://add-peko?noturl=https://x")).toBeNull();
      expect(parseDeepLink("peko://add-principal")).toBeNull();
      expect(parseDeepLink("peko://add-principal?noturl=https://x")).toBeNull();
    });

    it("rejects an empty url=", () => {
      // The plugin's URL parser treats `?url=` as `url === ""`, which
      // would surface a malformed URL to the modal: reject it here.
      expect(parseDeepLink("peko://add-peko?url=")).toBeNull();
      expect(parseDeepLink("peko://add-principal?url=")).toBeNull();
    });
  });

  describe("rejection paths", () => {
    it("rejects https URLs that don't match the share-link shape", () => {
      expect(parseDeepLink("https://pekohub.org/dashboard")).toBeNull();
      expect(parseDeepLink("https://pekohub.org/")).toBeNull();
      expect(parseDeepLink("https://pekohub.org/alice/x/y")).toBeNull();
    });

    it("rejects unsupported protocols", () => {
      expect(parseDeepLink("ftp://pekohub.org/alice/coding-assistant")).toBeNull();
      expect(parseDeepLink("javascript:alert(1)")).toBeNull();
      expect(parseDeepLink("file:///etc/passwd")).toBeNull();
    });

    it("rejects garbage strings that don't parse as a URL", () => {
      expect(parseDeepLink("not a url")).toBeNull();
      expect(parseDeepLink("")).toBeNull();
      expect(parseDeepLink("peko:add-peko?url=x")).toBeNull(); // missing //
    });
  });
});
