import js from "@eslint/js";
import { defineConfig, globalIgnores } from "eslint/config";
import reactHooks from "eslint-plugin-react-hooks";
import globals from "globals";
import tseslint from "typescript-eslint";

// What the dashboard's security rules forbid, so that a slip fails the lint
// rather than a review: the management key lives in sessionStorage only, the
// app sets no cookies, and the Content-Security-Policy allows no eval.
const securityRules = {
  "no-eval": "error",
  "no-new-func": "error",
  "no-script-url": "error",
  "no-restricted-globals": [
    "error",
    { name: "localStorage", message: "Nothing is kept in localStorage; the key lives in sessionStorage." },
  ],
  "no-restricted-properties": [
    "error",
    { object: "window", property: "localStorage", message: "Nothing is kept in localStorage." },
    { object: "globalThis", property: "localStorage", message: "Nothing is kept in localStorage." },
    { object: "document", property: "cookie", message: "The dashboard uses no cookies." },
    { object: "document", property: "write", message: "Render with React." },
  ],
  "no-restricted-syntax": [
    "error",
    {
      selector: "JSXAttribute[name.name='dangerouslySetInnerHTML']",
      message: "No raw HTML: render text with React.",
    },
    {
      selector: "AssignmentExpression > MemberExpression[property.name=/^(innerHTML|outerHTML)$/]",
      message: "No raw HTML: render text with React.",
    },
  ],
  "no-restricted-imports": [
    "error",
    {
      paths: [
        {
          name: "zod",
          message: "Import z from src/lib/zod, which turns off zod's eval-based parsers for the CSP.",
        },
      ],
    },
  ],
};

export default defineConfig([
  globalIgnores(["dist", "coverage", "test-results", "playwright-report"]),
  {
    files: ["**/*.{ts,tsx}"],
    extends: [
      js.configs.recommended,
      tseslint.configs.strictTypeChecked,
      tseslint.configs.stylisticTypeChecked,
    ],
    languageOptions: {
      parserOptions: {
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
      },
    },
    rules: {
      ...securityRules,
      "@typescript-eslint/restrict-template-expressions": ["error", { allowNumber: true }],
    },
  },
  {
    files: ["src/**/*.{ts,tsx}"],
    extends: [reactHooks.configs.flat.recommended],
    languageOptions: { globals: globals.browser },
  },
  {
    files: ["src/lib/zod.ts"],
    rules: { "no-restricted-imports": "off" },
  },
  {
    files: ["vite.config.ts", "build/**/*.ts", "e2e/**/*.ts", "playwright.config.ts"],
    languageOptions: { globals: globals.node },
  },
  {
    files: ["**/*.js"],
    extends: [js.configs.recommended],
    languageOptions: { globals: globals.node },
  },
]);
