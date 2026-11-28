// The playback half of the plugin interface: `player` and `danmaku`, as
// NipaPlay's plugins call them, with rates and offsets added. The host
// defines `__host` first (see plugin.rs): `has(permission)`, `read(name)`
// and `call(name, args)`. Calls are queued; nothing waits for the host.
var player = {
  play: function () {
    if (!__host.has('player.control')) return false;
    __host.call('playerPlay', []);
    return true;
  },
  pause: function () {
    if (!__host.has('player.control')) return false;
    __host.call('playerPause', []);
    return true;
  },
  seek: function (time) {
    if (!__host.has('player.control')) return false;
    __host.call('playerSeek', [Number(time)]);
    return true;
  },
  setRate: function (rate) {
    if (!__host.has('player.control')) return false;
    __host.call('playerSetRate', [Number(rate)]);
    return true;
  },
  getState: function () {
    if (!__host.has('player.control')) return null;
    return __host.read('player') || null;
  },
};

var danmaku = (function () {
  function allowed() {
    return __host.has('danmaku.modify');
  }
  function call(name, args) {
    if (!allowed()) return false;
    __host.call(name, args);
    return true;
  }

  // ---- Similarity, in the context itself ----

  function codePoints(text) {
    return Array.from(String(text == null ? '' : text)).slice(0, 200);
  }

  function levenshtein(a, b) {
    if (a.length === 0) return b.length;
    if (b.length === 0) return a.length;
    var previous = new Array(b.length + 1);
    var current = new Array(b.length + 1);
    for (var j = 0; j <= b.length; j++) previous[j] = j;
    for (var i = 1; i <= a.length; i++) {
      current[0] = i;
      for (var k = 1; k <= b.length; k++) {
        var cost = a[i - 1] === b[k - 1] ? 0 : 1;
        current[k] = Math.min(previous[k] + 1, current[k - 1] + 1, previous[k - 1] + cost);
      }
      var swap = previous;
      previous = current;
      current = swap;
    }
    return previous[b.length];
  }

  function bigrams(points) {
    var counts = {};
    for (var i = 0; i < points.length; i++) {
      var gram = (i === 0 ? '^' : points[i - 1]) + points[i];
      counts[gram] = (counts[gram] || 0) + 1;
    }
    return counts;
  }

  // The square of the cosine of the two bigram vectors, from 0 to 1.
  function cosine(a, b) {
    var p = bigrams(a);
    var q = bigrams(b);
    var dot = 0;
    var pp = 0;
    var qq = 0;
    for (var gram in p) {
      pp += p[gram] * p[gram];
      if (q[gram]) dot += p[gram] * q[gram];
    }
    for (var other in q) qq += q[other] * q[other];
    if (pp <= 0 || qq <= 0) return 0;
    return (dot * dot) / (pp * qq);
  }

  // How `a` resembles `b` under `config`: null, or the reason, distance and
  // score NipaPlay's engine reports.
  function compare(a, b, config) {
    if (a.join('') === b.join('')) {
      return { reason: 'identical', distance: 0, score: 1 };
    }
    var sum = a.length + b.length;
    var least = Math.max(1, config.maxDist * 2);
    var near = function (distance) {
      return sum < least ? distance < (config.maxDist * sum) / least : distance <= config.maxDist;
    };
    var edit = null;
    if (Math.abs(a.length - b.length) <= config.maxDist) {
      edit = levenshtein(a, b);
      if (near(edit)) {
        return { reason: 'edit_distance', distance: edit, score: 1 - edit / Math.max(a.length * 2, 1) };
      }
    }
    if (config.maxCosine <= 100 && !(edit !== null && edit >= sum)) {
      var similar = Math.floor(100 * cosine(a, b));
      if (similar >= config.maxCosine) {
        return { reason: 'cosine', distance: similar, score: similar / 100 };
      }
    }
    return null;
  }

  function checkSimilarity(list, options) {
    options = options || {};
    var config = {
      maxDist: options.max_dist == null ? 5 : Number(options.max_dist),
      maxCosine: options.max_cosine == null ? 45 : Number(options.max_cosine),
      crossMode: options.cross_mode !== false,
      timeWindow: options.time_window == null ? 30 : Number(options.time_window),
    };
    var items = (list || []).map(function (item) {
      return {
        points: codePoints(item.text != null ? item.text : item.content),
        mode: Number(item.mode || 0),
        time: Number(item.time_seconds != null ? item.time_seconds : item.time || 0),
      };
    });
    var pairs = [];
    var group = {};
    var groups = [];
    var kept = [];
    for (var i = 0; i < items.length; i++) {
      var item = items[i];
      var match = null;
      for (var k = kept.length - 1; k >= 0; k--) {
        var j = kept[k];
        if (config.timeWindow > 0 && item.time - items[j].time > config.timeWindow) break;
        if (!config.crossMode && item.mode !== items[j].mode) continue;
        var found = compare(item.points, items[j].points, config);
        if (found) {
          match = { index: j, found: found };
          break;
        }
      }
      if (!match) {
        kept.push(i);
        continue;
      }
      pairs.push({
        source_index: i,
        target_index: match.index,
        reason: match.found.reason,
        distance: match.found.distance,
        score: match.found.score,
      });
      var at = group[match.index];
      if (at === undefined) {
        at = groups.length;
        groups.push([match.index]);
        group[match.index] = at;
      }
      groups[at].push(i);
      group[i] = at;
    }
    return { pairs: pairs, groups: groups };
  }

  function pairSimilarity(textA, textB) {
    var a = codePoints(textA);
    var b = codePoints(textB);
    if (a.join('') === b.join('')) return 1;
    var longest = Math.max(a.length, b.length, 1);
    return Math.max(0, 1 - levenshtein(a, b) / longest);
  }

  return {
    show: function () {
      return call('danmakuShow', []);
    },
    hide: function () {
      return call('danmakuHide', []);
    },
    setOpacity: function (opacity) {
      return call('danmakuSetOpacity', [Number(opacity)]);
    },
    addFilter: function (filterId, pattern) {
      return call('danmakuAddFilter', [String(filterId), pattern == null ? String(filterId) : String(pattern)]);
    },
    removeFilter: function (filterId) {
      return call('danmakuRemoveFilter', [String(filterId)]);
    },
    replace: function (data) {
      if (typeof data === 'string') data = JSON.parse(data);
      return call('danmakuReplace', [data]);
    },
    setOffset: function (seconds) {
      return call('danmakuSetOffset', [Number(seconds)]);
    },
    getOffset: function () {
      if (!allowed()) return null;
      return Number(__host.read('danmaku.offset') || 0);
    },
    similarityAvailable: function () {
      return true;
    },
    checkSimilarity: function (list, config) {
      if (!allowed()) return null;
      return checkSimilarity(list, config);
    },
    pairSimilarity: function (textA, textB) {
      if (!allowed()) return 0;
      return pairSimilarity(textA, textB);
    },
  };
})();
