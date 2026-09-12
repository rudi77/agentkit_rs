// Minimal behaviour: mobile navigation toggle. No framework, no build step.
(function () {
  var btn = document.querySelector('.nav-toggle');
  var nav = document.querySelector('nav.main');
  if (!btn || !nav) return;
  btn.addEventListener('click', function () {
    var open = nav.classList.toggle('open');
    btn.setAttribute('aria-expanded', open ? 'true' : 'false');
  });
})();
