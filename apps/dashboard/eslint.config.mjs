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
];

export default eslintConfig;
