import { describe, it, expect } from "vitest";
import { shareUrlFor, deepLinkFor } from "../lib/discovery";
import { parseShareUrl, shareLinkFromInput } from "../hooks/useRemotePrincipals";

/**
 * The share links the desktop emits and reads. A peko's page on the hub's
 * website is `/owner/name` (pekohub ADR-009, amended 2026-11-28), wrapped
 * in `peko://add-peko` for "Add to desktop". The retired pages
 * (`/@owner/name`, `/peko/…`, `/p/…`) no longer parse.
 */
describe("shareUrlFor", () => {
  it("names the peko's page on the official website, not the API", () => {
    expect(shareUrlFor("https://api.pekohub.ai", { owner: "alice", name: "coding-assistant" })).toBe(
      "https://pekohub.ai/alice/coding-assistant",
    );
  });

  it("keeps a self-hosted hub's origin", () => {
    expect(shareUrlFor("http://localhost:8080/", { owner: "alice", name: "p" })).toBe(
      "http://localhost:8080/alice/p",
    );
  });

  it("encodes a segment that would break the path", () => {
    expect(shareUrlFor("https://hub.example.com", { owner: "alice/bob", name: "my agent" })).toBe(
      "https://hub.example.com/alice%2Fbob/my%20agent",
    );
  });
});

describe("deepLinkFor", () => {
  it("wraps the share link in peko://add-peko, and it parses back", () => {
    const link = deepLinkFor("https://api.pekohub.ai", { owner: "alice", name: "p" });
    expect(link).toBe("peko://add-peko?url=https%3A%2F%2Fpekohub.ai%2Falice%2Fp");
    const inner = decodeURIComponent(link.split("?url=")[1]);
    expect(parseShareUrl(inner)).toMatchObject({ owner: "alice", principalName: "p" });
  });
});

describe("parseShareUrl", () => {
  it("reads the /owner/name page, the owner in any case", () => {
    for (const raw of ["https://pekohub.ai/alice/ada", "https://pekohub.ai/Alice/ada/"]) {
      expect(parseShareUrl(raw)).toEqual({
        hubUrl: "https://pekohub.ai",
        owner: "alice",
        principalName: "ada",
        inviteToken: undefined,
      });
    }
    expect(parseShareUrl("https://pekohub.ai/alice/ada?token=t")?.inviteToken).toBe("t");
  });

  it("reads the API URL forms", () => {
    for (const path of ["/v1/public/pekos/alice/ada", "/v1/public/principals/alice/ada"]) {
      expect(parseShareUrl(`https://pekohub.ai${path}`)).toMatchObject({ owner: "alice", principalName: "ada" });
    }
  });

  it("rejects the retired pages, a person's page, a seed's page and anything else", () => {
    for (const raw of [
      "https://pekohub.ai/@alice/ada",
      "https://pekohub.ai/peko/alice/ada",
      "https://pekohub.ai/p/alice/ada",
      "https://pekohub.ai/alice",
      "https://pekohub.ai/alice/seeds/ada",
      "https://pekohub.ai/alice/a/b",
      "@alice/ada",
    ]) {
      expect(parseShareUrl(raw)).toBeNull();
    }
  });
});

describe("shareLinkFromInput", () => {
  it("turns a typed peko ID into its page on the configured hub", () => {
    expect(shareLinkFromInput(" @Alice/ada ", "https://api.pekohub.ai")).toBe("https://pekohub.ai/alice/ada");
    expect(shareLinkFromInput("@alice/ada", "http://localhost:3000")).toBe("http://localhost:3000/alice/ada");
  });

  it("leaves a link, or anything else, as typed", () => {
    expect(shareLinkFromInput("https://pekohub.ai/alice/ada", "https://api.pekohub.ai")).toBe(
      "https://pekohub.ai/alice/ada",
    );
    expect(shareLinkFromInput("@alice", "https://api.pekohub.ai")).toBe("@alice");
  });
});
