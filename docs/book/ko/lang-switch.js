// Language switcher for the wsPrism books. The English book is served at the
// site root, the others under /ko/ and /zh/, and every book has the same file
// layout, so the same page exists in each language.
(function () {
  var langs = [['en', 'English'], ['ko', '한국어'], ['zh', '简体中文']];
  var current = (document.documentElement.lang || 'en').slice(0, 2);
  var bookRoot = new URL(typeof path_to_root === 'string' ? path_to_root : './', window.location.href);
  var siteRoot = current === 'en' ? bookRoot : new URL('../', bookRoot);
  var page = window.location.href.slice(bookRoot.href.length).split('#')[0];
  var bar = document.querySelector('.right-buttons');
  if (!bar) return;
  var box = document.createElement('span');
  box.className = 'wsprism-lang';
  box.style.cssText = 'margin: 0 0.8em; font-size: 0.85em; white-space: nowrap;';
  langs.forEach(function (l, i) {
    if (i) box.appendChild(document.createTextNode(' · '));
    if (l[0] === current) {
      var b = document.createElement('strong');
      b.textContent = l[1];
      box.appendChild(b);
    } else {
      var a = document.createElement('a');
      a.href = new URL((l[0] === 'en' ? '' : l[0] + '/') + page, siteRoot).href;
      a.textContent = l[1];
      box.appendChild(a);
    }
  });
  bar.insertBefore(box, bar.firstChild);
})();
