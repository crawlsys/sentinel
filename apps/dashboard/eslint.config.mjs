import { FlatCompat } from "@eslint/eslintrc";
import { dirname } from "path";
import { fileURLToPath } from "url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

const compat = new FlatCompat({
  baseDirectory: __dirname,
});

// Domain layer must stay framework-free (SEN-22). No React, Next, MUI, or
// emotion. Caught early with a flat-config override scoped to src/domain/.
//
// Ports layer (SEN-23) has the same constraint — interface declarations
// only, zero IO and zero framework imports.
const FRAMEWORK_PATTERNS = [
  "react",
  "react-dom",
  "react/*",
  "react-dom/*",
  "next",
  "next/*",
  "@mui/*",
  "@emotion/*",
];

// Atomic-design boundary (SEN-26). Atoms must remain leaf components — they
// can only depend on React, MUI, the design tokens at src/theme, and on each
// other (relative imports are siblings within the atoms folder).
const ATOM_FORBIDDEN_PATTERNS = [
  "**/molecules/*",
  "**/molecules",
  "**/organisms/*",
  "**/organisms",
  "**/templates/*",
  "**/templates",
  "**/application/*",
  "**/application",
  "**/adapters/*",
  "**/adapters",
  "@/components/molecules",
  "@/components/molecules/*",
  "@/components/organisms",
  "@/components/organisms/*",
  "@/components/templates",
  "@/components/templates/*",
  "@/application",
  "@/application/*",
  "@/adapters",
  "@/adapters/*",
];

const eslintConfig = [
  ...compat.extends("next/core-web-vitals", "next/typescript"),
  {
    ignores: [".next/**", "node_modules/**", "dist/**"],
  },
  {
    files: ["src/domain/**/*.ts"],
    rules: {
      "no-restricted-imports": [
        "error",
        {
          patterns: FRAMEWORK_PATTERNS.map((p) => ({
            group: [p],
            message:
              "src/domain/** must stay framework-free (no React/Next/MUI/emotion). Move framework code to src/components/, src/adapters/, or src/application/.",
          })),
        },
      ],
    },
  },
  {
    files: ["src/ports/**/*.ts"],
    rules: {
      "no-restricted-imports": [
        "error",
        {
          patterns: [
            ...FRAMEWORK_PATTERNS.map((p) => ({
              group: [p],
              message:
                "src/ports/** must stay framework-free (no React/Next/MUI/emotion). Ports declare interfaces only — keep IO/framework code in src/adapters/.",
            })),
            {
              group: ["fs", "fs/*", "node:fs", "node:fs/*", "path", "node:path", "axios", "node-fetch"],
              message:
                "src/ports/** must declare interfaces only — no IO imports. Move concrete IO to src/adapters/.",
            },
          ],
        },
      ],
    },
  },
  {
    files: ["src/components/atoms/**/*.{ts,tsx}"],
    rules: {
      "no-restricted-imports": [
        "error",
        {
          patterns: ATOM_FORBIDDEN_PATTERNS.map((p) => ({
            group: [p],
            message:
              "Atoms must not depend on molecules/organisms/templates/application/adapters. Atomic design boundary — see src/components/atoms/index.ts.",
          })),
        },
      ],
    },
  },
];

export default eslintConfig;
