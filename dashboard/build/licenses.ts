// Writes dist/third-party-licenses.txt: every npm package whose code ends up
// in the built app, with its license text. The app's About page links to it.
//
// A build fails when a bundled package's license isn't one this project
// accepts (MIT, ISC, Apache-2.0 or BSD), or when its package can't be read.

import { existsSync, readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import type { Plugin } from "vite";

/** The file written next to index.html. */
export const LICENSES_FILE = "third-party-licenses.txt";

/** The SPDX identifiers a bundled package may be licensed under. */
export const ALLOWED_LICENSES: ReadonlySet<string> = new Set([
  "MIT",
  "ISC",
  "Apache-2.0",
  "BSD-2-Clause",
  "BSD-3-Clause",
  "0BSD",
]);

/** Bundler helpers that reach the bundle as virtual modules, by id prefix. */
const VIRTUAL_MODULE_PACKAGES: readonly (readonly [string, string])[] = [
  ["vite/", "vite"],
  ["rolldown/", "rolldown"],
];

/** A bundled package and what its license says. */
export interface PackageLicense {
  name: string;
  version: string;
  license: string;
  source: string | null;
  texts: { file: string; text: string }[];
}

/**
 * The directory of the npm package holding module `id`, or null when the
 * module isn't from one (the app's own code). Virtual helper modules map to
 * the package that makes them, under `root`'s node_modules.
 */
export function packageDirOf(id: string, root: string): string | null {
  let file = id.split("?")[0] ?? "";
  if (file.startsWith("\0")) {
    file = file.slice(1);
    for (const [prefix, name] of VIRTUAL_MODULE_PACKAGES) {
      if (file.startsWith(prefix)) {
        return findPackageDir(name, root);
      }
    }
    return null;
  }
  const normalized = file.replaceAll("\\", "/");
  const marker = "/node_modules/";
  const at = normalized.lastIndexOf(marker);
  if (at < 0) {
    return null;
  }
  const rest = normalized.slice(at + marker.length).split("/");
  const first = rest[0] ?? "";
  const nameParts = first.startsWith("@") ? rest.slice(0, 2) : rest.slice(0, 1);
  if (nameParts.length === 0 || nameParts.some((part) => part === "")) {
    return null;
  }
  return path.normalize(normalized.slice(0, at + marker.length) + nameParts.join("/"));
}

/** Where package `name` is installed for `root`, hoisted or under vite. */
function findPackageDir(name: string, root: string): string {
  const candidates = [
    path.join(root, "node_modules", name),
    path.join(root, "node_modules", "vite", "node_modules", name),
  ];
  return candidates.find((dir) => existsSync(path.join(dir, "package.json"))) ?? candidates[0] ?? "";
}

/**
 * Whether an SPDX license expression allows use under this project's rules:
 * for `A OR B` any one choice must be allowed, for `A AND B` every part.
 */
export function licenseIsAllowed(expression: string): boolean {
  const bare = expression.replaceAll("(", " ").replaceAll(")", " ").trim();
  if (bare === "") {
    return false;
  }
  if (/\sOR\s/.test(bare)) {
    return bare.split(/\s+OR\s+/).some((choice) => licenseIsAllowed(choice));
  }
  return bare.split(/\s+AND\s+/).every((part) => ALLOWED_LICENSES.has(part.trim()));
}

/** The license expression a package.json declares, or "" if none. */
function declaredLicense(manifest: Record<string, unknown>): string {
  const license = manifest.license;
  if (typeof license === "string") {
    return license;
  }
  if (license !== null && typeof license === "object" && "type" in license) {
    const type = license.type;
    return typeof type === "string" ? type : "";
  }
  const licenses = manifest.licenses;
  if (Array.isArray(licenses)) {
    const types = licenses
      .map((entry: unknown) =>
        entry !== null && typeof entry === "object" && "type" in entry
          ? String(entry.type)
          : "",
      )
      .filter((type) => type !== "");
    return types.join(" OR ");
  }
  return "";
}

/** The package's repository URL, if it names one. */
function declaredSource(manifest: Record<string, unknown>): string | null {
  const repository = manifest.repository;
  if (typeof repository === "string") {
    return repository.replace(/^git\+/, "");
  }
  if (repository !== null && typeof repository === "object" && "url" in repository) {
    const url = repository.url;
    return typeof url === "string" ? url.replace(/^git\+/, "") : null;
  }
  return null;
}

/** License, notice and copying files at the top of a package, by name. */
const LICENSE_FILE = /^(licen[cs]e|copying|notice)([.-].*)?$/i;

/** Reads a package's name, version, license and license files. */
export function readPackageLicense(dir: string): PackageLicense {
  const manifestPath = path.join(dir, "package.json");
  if (!existsSync(manifestPath)) {
    throw new Error(`no package.json in ${dir}`);
  }
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8")) as Record<string, unknown>;
  const name = typeof manifest.name === "string" ? manifest.name : path.basename(dir);
  const version = typeof manifest.version === "string" ? manifest.version : "unknown";
  const license = declaredLicense(manifest);
  if (!licenseIsAllowed(license)) {
    throw new Error(
      `${name}@${version} is licensed under "${license || "nothing declared"}", ` +
        "which isn't MIT, ISC, Apache-2.0 or BSD",
    );
  }
  const texts = readdirSync(dir, { withFileTypes: true })
    .filter((entry) => entry.isFile() && LICENSE_FILE.test(entry.name))
    .map((entry) => entry.name)
    .sort()
    .map((file) => ({ file, text: readFileSync(path.join(dir, file), "utf8").trim() }));
  return { name, version, license, source: declaredSource(manifest), texts };
}

/** The text of third-party-licenses.txt for `packages`. */
export function renderLicenses(packages: readonly PackageLicense[]): string {
  const rule = "-".repeat(78);
  const sorted = [...packages].sort((a, b) =>
    a.name === b.name ? a.version.localeCompare(b.version) : a.name.localeCompare(b.name),
  );
  const lines = [
    "open-ferry dashboard: third-party software",
    "",
    "The open-ferry web dashboard includes code from the npm packages below.",
    "Each is listed with its license and the license text it ships with.",
    "",
  ];
  for (const pkg of sorted) {
    lines.push(rule, `${pkg.name} ${pkg.version}`, `License: ${pkg.license}`);
    if (pkg.source !== null) {
      lines.push(`Source: ${pkg.source}`);
    }
    lines.push("");
    if (pkg.texts.length === 0) {
      lines.push(
        `The package ships no license file; its package.json declares ${pkg.license}.`,
        "",
      );
    }
    for (const { file, text } of pkg.texts) {
      lines.push(`[${file}]`, "", text, "");
    }
  }
  lines.push(rule, "");
  return lines.join("\n");
}

/**
 * The plugin. `include` names packages whose code reaches the build without
 * being a module of the bundle, such as tailwindcss, whose base styles are
 * written into the CSS.
 */
export function thirdPartyLicenses(options: { root: string; include?: readonly string[] }): Plugin {
  return {
    name: "open-ferry:third-party-licenses",
    apply: "build",
    generateBundle(_outputOptions, bundle) {
      const dirs = new Set<string>();
      for (const item of Object.values(bundle)) {
        if (item.type !== "chunk") {
          continue;
        }
        for (const id of item.moduleIds) {
          const dir = packageDirOf(id, options.root);
          if (dir !== null) {
            dirs.add(dir);
          }
        }
      }
      for (const name of options.include ?? []) {
        dirs.add(findPackageDir(name, options.root));
      }
      const packages = new Map<string, PackageLicense>();
      for (const dir of dirs) {
        try {
          const pkg = readPackageLicense(dir);
          packages.set(`${pkg.name}@${pkg.version}`, pkg);
        } catch (error) {
          this.error(error instanceof Error ? error.message : String(error));
        }
      }
      this.emitFile({
        type: "asset",
        fileName: LICENSES_FILE,
        source: renderLicenses([...packages.values()]),
      });
    },
  };
}
