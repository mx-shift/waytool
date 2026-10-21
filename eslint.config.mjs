// ESLint config for the gnome-shell extension (CI runs `npx eslint`).
// Self-contained — no plugins or shared configs — so it needs nothing
// installed beyond eslint itself.

export default [
    {
        files: ['gnome-extension/**/*.js'],
        languageOptions: {
            ecmaVersion: 2024,
            sourceType: 'module',
            globals: {
                // Provided by gnome-shell / gjs.
                global: 'readonly',
                console: 'readonly',
                print: 'readonly',
                log: 'readonly',
                logError: 'readonly',
                TextDecoder: 'readonly',
                TextEncoder: 'readonly',
            },
        },
        rules: {
            'no-undef': 'error',
            'no-unused-vars': ['error', {argsIgnorePattern: '^_', caughtErrorsIgnorePattern: '^_'}],
            'no-unreachable': 'error',
            'no-dupe-keys': 'error',
            'no-dupe-class-members': 'error',
            'no-redeclare': 'error',
            'no-const-assign': 'error',
            'no-self-assign': 'error',
            'no-unsafe-finally': 'error',
            'require-await': 'error',
            'prefer-const': 'error',
            'eqeqeq': 'error',
        },
    },
];
