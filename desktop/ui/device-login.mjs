export function deviceLoginView(state, now = Date.now()) {
  const device = state?.login?.pending && state.login.method === "device_code";
  const prompt = device ? state.device_code : null;
  const remaining = Math.max(0, Math.ceil(((prompt?.expires_at || 0) * 1000 - now) / 1000));
  return {
    title: device ? "Вход по одноразовому коду" : "Остался один шаг — вход в браузере",
    description: device
      ? prompt ? "Откройте страницу на любом устройстве и введите код. Текущий вход сохранится до подтверждения."
        : "Получаем код для входа… Текущая сессия сохраняется."
      : "Текущая сессия сохраняется до успешного входа.",
    code: prompt?.user_code || "",
    url: prompt?.verification_url || "",
    expiry: !prompt ? "" : remaining
      ? `Код действует ещё ${Math.floor(remaining / 60)}:${String(remaining % 60).padStart(2, "0")}`
      : "Срок действия кода истёк. Отмените вход и получите новый код.",
  };
}
