// @vitest-environment node
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterEach, describe, expect, it } from "vitest";

import { licenseIsAllowed, packageDirOf, readPackageLicense, renderLicenses } from "./licenses";

const temps: string[] = [];
afterEach(() => {
  for (const dir of temps.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

function tempPackage(manifest: Record<string, unknown>, files: Record<string, string> = {}) {
  const dir = mkdtempSync(path.join(tmpdir(), "of-licenses-"));
  temps.push(dir);
  writeFileSync(path.join(dir, "package.json"), JSON.stringify(manifest));
  for (const [name, text] of Object.entries(files)) {
    writeFileSync(path.join(dir, name), text);
  }
  return dir;
}

describe("licenseIsAllowed", () => {
  it("allows the permissive licenses", () => {
    for (const id of ["MIT", "ISC", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "0BSD"]) {
      expect(licenseIsAllowed(id), id).toBe(true);
    }
  });

  it("needs one allowed choice for OR and every part for AND", () => {
    expect(licenseIsAllowed("(MIT OR GPL-3.0)")).toBe(true);
    expect(licenseIsAllowed("MIT AND ISC")).toBe(true);
    expect(licenseIsAllowed("MIT AND GPL-3.0")).toBe(false);
    expect(licenseIsAllowed("(MIT AND CC-BY-4.0) OR Apache-2.0")).toBe(true);
  });

  it("refuses the rest, and nothing declared", () => {
    for (const id of ["GPL-3.0", "LGPL-2.1", "MPL-2.0", "UNLICENSED", "CC-BY-4.0", "", "  "]) {
      expect(licenseIsAllowed(id), id).toBe(false);
    }
  });
});

describe("packageDirOf", () => {
  const root = path.join(tmpdir(), "app");

  it("finds plain and scoped packages, nested or not", () => {
    expect(packageDirOf("/w/node_modules/react/index.js", root)).toBe(
      path.normalize("/w/node_modules/react"),
    );
    expect(packageDirOf("C:\\w\\node_modules\\@tanstack\\query-core\\build\\x.js?v=1", root)).toBe(
      path.normalize("C:/w/node_modules/@tanstack/query-core"),
    );
    expect(packageDirOf("/w/node_modules/a/node_modules/b/lib/c.js", root)).toBe(
      path.normalize("/w/node_modules/a/node_modules/b"),
    );
  });

  it("leaves the app's own modules out", () => {
    expect(packageDirOf("/w/dashboard/src/main.tsx", root)).toBeNull();
    expect(packageDirOf("\0virtual:something", root)).toBeNull();
  });

  it("maps bundler helpers to their package", () => {
    expect(packageDirOf("\0vite/modulepreload-polyfill.js", root)).toContain("vite");
    expect(packageDirOf("\0rolldown/runtime.js", root)).toContain("rolldown");
  });
});

describe("readPackageLicense and renderLicenses", () => {
  it("reads the license and its files", () => {
    const dir = tempPackage(
      { name: "demo", version: "1.2.3", license: "MIT", repository: "git+https://example.test/demo.git" },
      { LICENSE: "MIT License\n\nCopyright demo", "README.md": "not a license" },
    );
    const pkg = readPackageLicense(dir);
    expect(pkg).toEqual({
      name: "demo",
      version: "1.2.3",
      license: "MIT",
      source: "https://example.test/demo.git",
      texts: [{ file: "LICENSE", text: "MIT License\n\nCopyright demo" }],
    });
    const text = renderLicenses([pkg]);
    expect(text).toContain("demo 1.2.3");
    expect(text).toContain("License: MIT");
    expect(text).toContain("Copyright demo");
    expect(text).not.toContain("not a license");
  });

  it("fails on a license that isn't allowed", () => {
    const dir = tempPackage({ name: "copyleft", version: "1.0.0", license: "GPL-3.0" });
    expect(() => readPackageLicense(dir)).toThrow(/copyleft@1.0.0 is licensed under "GPL-3.0"/);
  });

  it("fails on a package without package.json", () => {
    const dir = mkdtempSync(path.join(tmpdir(), "of-licenses-"));
    temps.push(dir);
    mkdirSync(path.join(dir, "lib"));
    expect(() => readPackageLicense(dir)).toThrow(/no package.json/);
  });
});
