import js from "@eslint/js";
import globals from "globals";

export default [
  {
    ignores: ["node_modules", "out-site", "src/.observablehq"],
  },
  js.configs.recommended,
  {
    files: [
      "src/data/**/*.js",
      "scripts/**/*.js",
      "test/**/*.js",
      "observablehq.config.js",
      "vitest.config.js",
      "eslint.config.js",
    ],
    languageOptions: {
      globals: globals.node,
    },
  },
  {
    files: ["src/components/**/*.js"],
    languageOptions: {
      globals: globals.browser,
    },
  },
];
