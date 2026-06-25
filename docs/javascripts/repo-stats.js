(function () {
  const REPO = "KovantAI/fermut";
  const API = "https://api.github.com";

  function fmt(n) {
    if (n >= 1000) return (n / 1000).toFixed(1).replace(/\.0$/, "") + "k";
    return String(n);
  }

  function svg(path) {
    return (
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" ' +
      'fill="currentColor" class="md-source__icon md-icon">' +
      '<path d="' + path + '"/></svg>'
    );
  }

  const STAR_PATH =
    "M12 17.27 18.18 21l-1.64-7.03L22 9.24l-7.19-.61L12 2 9.19 8.63 2 9.24l5.46 4.73L5.82 21z";
  const PR_PATH =
    "M6 3a3 3 0 0 1 1 5.83v6.34A3 3 0 1 1 5 15.17V8.83A3 3 0 0 1 6 3m12 13.17V8a3 3 0 0 0-3-3h-2V2L9 6l4 4V7h2a1 1 0 0 1 1 1v8.17a3 3 0 1 0 2 0";

  async function fetchStats() {
    try {
      const [repo, pulls] = await Promise.all([
        fetch(API + "/repos/" + REPO).then((r) => (r.ok ? r.json() : null)),
        fetch(
          API + "/search/issues?q=repo:" + REPO + "+is:pr+is:open&per_page=1"
        ).then((r) => (r.ok ? r.json() : null)),
      ]);
      return {
        stars: repo ? repo.stargazers_count : null,
        prs: pulls ? pulls.total_count : null,
      };
    } catch (_) {
      return { stars: null, prs: null };
    }
  }

  function render(stats) {
    document.querySelectorAll(".md-source__repository").forEach((repo) => {
      let facts = repo.querySelector(".md-source__facts");
      if (!facts) {
        facts = document.createElement("ul");
        facts.className = "md-source__facts";
        repo.appendChild(facts);
      }
      facts.innerHTML = "";

      if (stats.stars !== null) {
        const li = document.createElement("li");
        li.className = "md-source__fact md-source__fact--stars";
        li.innerHTML = svg(STAR_PATH) + " " + fmt(stats.stars);
        facts.appendChild(li);
      }
      if (stats.prs !== null) {
        const li = document.createElement("li");
        li.className = "md-source__fact md-source__fact--prs";
        li.innerHTML = svg(PR_PATH) + " " + fmt(stats.prs) + " PRs";
        facts.appendChild(li);
      }
    });
  }

  function init() {
    fetchStats().then(render);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }

  if (window.document$ && typeof window.document$.subscribe === "function") {
    window.document$.subscribe(init);
  }
})();
