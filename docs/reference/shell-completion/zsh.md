# zsh

```sh
fermut completions zsh > "${fpath[1]}/_fermut"
```

If your `$fpath` doesn't include a writable directory, add one in
`.zshrc`:

```sh
mkdir -p ~/.zsh/completions
fpath=(~/.zsh/completions $fpath)            # in .zshrc
fermut completions zsh > ~/.zsh/completions/_fermut
```

Then run `compinit` or restart your shell.
