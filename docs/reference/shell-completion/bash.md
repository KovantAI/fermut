# bash

System-wide:

```sh
fermut completions bash > /usr/local/etc/bash_completion.d/fermut
```

Current user only:

```sh
fermut completions bash > ~/.local/share/bash-completion/completions/fermut
```

Reload your shell or `source` the file. Regenerate after upgrading
fermut.
