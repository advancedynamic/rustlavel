/*
 * Turns every <canvas data-chart="..."> on the page into a Chart.js chart.
 *
 * This file exists because a Content-Security-Policy worth having forbids
 * inline script, so `new Chart(...)` cannot be written into the page beside
 * the data. The configuration travels in an escaped attribute instead and is
 * read back here.
 */
(function () {
  "use strict";

  function ticks(scale) {
    // `rustlavelTicks` is ours, not Chart.js's: a prefix and a suffix survive
    // JSON, and the callback that applies them cannot.
    var spec = scale && scale.rustlavelTicks;
    if (!spec) return;
    delete scale.rustlavelTicks;
    var prefix = spec.prefix || "";
    var suffix = spec.suffix || "";
    scale.ticks = Object.assign({}, scale.ticks, {
      callback: function (value) {
        return prefix + value.toLocaleString() + suffix;
      },
    });
  }

  function draw(canvas) {
    if (canvas.dataset.chartDrawn === "true") return;

    var config;
    try {
      config = JSON.parse(canvas.dataset.chart);
    } catch (error) {
      // A chart that cannot be parsed must not take the page down, and must
      // not fail silently either.
      console.error("rustlavel-chart: could not read the chart configuration", error, canvas);
      return;
    }

    if (config.options && config.options.scales) {
      Object.keys(config.options.scales).forEach(function (name) {
        ticks(config.options.scales[name]);
      });
    }

    if (typeof Chart === "undefined") {
      console.error("rustlavel-chart: chart.umd.js has not loaded; is the Charts plugin registered?");
      return;
    }

    // Follow the page into dark mode. Chart.js draws its own labels and grid
    // lines, and its defaults are for a white page.
    var styles = getComputedStyle(document.documentElement);
    var ink = styles.getPropertyValue("--chart-ink").trim();
    var grid = styles.getPropertyValue("--chart-grid").trim();
    if (ink) Chart.defaults.color = ink;
    if (grid) Chart.defaults.borderColor = grid;

    canvas.dataset.chartDrawn = "true";
    new Chart(canvas, config);
  }

  function drawAll(root) {
    (root || document).querySelectorAll("canvas[data-chart]").forEach(draw);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", function () {
      drawAll(document);
    });
  } else {
    drawAll(document);
  }

  // A chart swapped in later — a tab, a fragment fetched after load — draws
  // itself without the page having to call anything.
  if (typeof MutationObserver !== "undefined") {
    new MutationObserver(function (records) {
      records.forEach(function (record) {
        record.addedNodes.forEach(function (node) {
          if (node.nodeType !== 1) return;
          if (node.matches && node.matches("canvas[data-chart]")) draw(node);
          else drawAll(node);
        });
      });
    }).observe(document.documentElement, { childList: true, subtree: true });
  }

  window.rustlavelCharts = { draw: draw, drawAll: drawAll };
})();
